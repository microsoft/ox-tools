// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub(super) fn runtime_fixture() -> (MemoryFiles, OwnedUnit, Command, BTreeMap<String, String>) {
    let (files, unit) = fixture("unit");
    let root = drive().join("runtime");
    let source = root.join("src").join("lib.rs");
    files.put(
        root.join("Cargo.toml"),
        "[package]\nname='cargo-gamma-rt'\nversion='0.1.0'\nbuild=false\n[lib]\nname='gamma_rt'\nproc-macro=false\n",
    );
    files.put(&source, "pub fn selected() -> bool { false }");
    let metadata = unit.target.join("libgamma_rt-runtime.rmeta");
    let archive = unit.target.join("libgamma_rt-runtime.rlib");
    files.put(&metadata, b"rust\0runtime metadata");
    files.put(&archive, b"!<arch>\nruntime archive");
    files.put(
        unit.target.join("gamma_rt-runtime.d"),
        format!(
            "phony:\n{metadata}: {source}\n{archive}: {source}\n{}: {source}\n\n{source}:\n",
            unit.target.join("runtime.o")
        ),
    );
    let mut command = Command::new(&unit.compiler);
    let _command = command.args([
        OsString::from("--crate-name=gamma_rt"),
        OsString::from("--crate-type=lib"),
        OsString::from("--emit=dep-info,metadata,link"),
        OsString::from("--out-dir"),
        unit.target.clone().into(),
        OsString::from("-Cextra-filename=-runtime"),
        source.into(),
    ]);
    let environment = BTreeMap::from([
        (RUNTIME_VAR.into(), root.to_string()),
        (TARGET_VAR.into(), unit.target.to_string()),
        ("CARGO_MANIFEST_DIR".into(), root.to_string()),
        (COMMAND_VAR.into(), "parent command".into()),
    ]);
    (files, unit, command, environment)
}

#[test]
fn runtime_capture_declines_an_inventory_lost_after_reading_its_sources() {
    let (files, unit, command, environment) = runtime_fixture();
    let _capture = runtime::capture_with(&files, &command, &environment, &unit.cwd).unwrap();
    let mut script = snapshot_io::Script::from_files(
        files
            .0
            .borrow()
            .iter()
            .map(|(path, bytes)| (path.clone().into_std_path_buf(), bytes.clone())),
    );
    let _previous = script.directory_reads.insert(
        std::path::PathBuf::from(&environment[RUNTIME_VAR]),
        [Ok(()), Err(io::ErrorKind::PermissionDenied)].into(),
    );
    snapshot_io::with(script, || {
        assert!(runtime::capture_with(&NativeFiles, &command, &environment, &unit.cwd).is_none());
    });
}

fn alter_command(command: &Command, transform: impl FnOnce(&mut Vec<OsString>)) -> Command {
    let mut args: Vec<_> = command.get_args().map(OsString::from).collect();
    transform(&mut args);
    let mut altered = Command::new(command.get_program());
    let _altered = altered.args(args);
    altered
}

#[test]
fn native_runtime_capture_publishes_provenance_only_with_an_owned_directory_and_cwd() {
    use super::snapshot_io::Script;

    let (files, unit, command, environment) = runtime_fixture();
    let directory = unit.captures.parent().unwrap().parent().unwrap();
    for (cwd, owned) in [(true, true), (false, true), (true, false)] {
        let mut script = Script::from_files(
            files
                .0
                .borrow()
                .iter()
                .map(|(path, bytes)| (path.clone().into_std_path_buf(), bytes.clone())),
        );
        script.cwd = cwd.then(|| unit.cwd.clone().into_std_path_buf());
        let mut environment = environment.clone();
        if owned {
            let _previous = environment.insert(CAPTURE_DIR_VAR.into(), directory.to_string());
        }
        snapshot_io::with(script, || {
            assert!(runtime::load(&NativeFiles, directory).is_none());
            assert_eq!(runtime::capture(&command, &environment), (cwd && owned).then_some(()));
            let capture = runtime::load(&NativeFiles, directory);
            if cwd && owned {
                let capture = capture.expect("the complete owned runtime capture is published");
                assert!(capture.valid(&NativeFiles, Utf8Path::new(&environment[RUNTIME_VAR]), Some(&unit.compiler)));
                assert!(capture.contains(&NativeFiles, &unit.target.join("libgamma_rt-runtime.rmeta")));
            } else {
                assert!(capture.is_none());
            }
        });
    }
}

#[test]
fn runtime_provenance_cannot_be_rebound_to_an_ancestor_with_the_same_file_inventory() {
    let (files, unit, command, mut environment) = runtime_fixture();
    let parent = Utf8PathBuf::from(&environment[RUNTIME_VAR]);
    let root = parent.join("owned");
    let moved = files
        .0
        .borrow()
        .iter()
        .filter_map(|(path, bytes)| {
            path.strip_prefix(&parent)
                .ok()
                .map(|relative| (path.clone(), root.join(relative), bytes.clone()))
        })
        .collect::<Vec<_>>();
    for (old, new, bytes) in moved {
        let _removed = files.0.borrow_mut().remove(&old);
        files.put(new, bytes);
    }
    let dep = unit.target.join("gamma_rt-runtime.d");
    let text = String::from_utf8(files.read(&dep).unwrap())
        .unwrap()
        .replace(parent.as_str(), root.as_str());
    files.put(dep, text);
    let command = alter_command(&command, |args| *args.last_mut().unwrap() = root.join("src").join("lib.rs").into());
    let _previous = environment.insert(RUNTIME_VAR.into(), root.to_string());
    let _previous = environment.insert("CARGO_MANIFEST_DIR".into(), root.to_string());
    let capture = runtime::capture_with(&files, &command, &environment, &unit.cwd).unwrap();
    assert_eq!(
        inventory(&files, &root, Utf8Path::new("")),
        inventory(&files, &parent, Utf8Path::new(""))
    );
    assert!(capture.valid(&files, &root, Some(&unit.compiler)));
    assert!(!capture.valid(&files, &parent, Some(&unit.compiler)));
}

#[test]
fn transitive_runtime_provenance_from_an_older_target_cannot_close_a_new_target_snapshot() {
    let (files, mut unit, command, environment) = runtime_fixture();
    let runtime = runtime::capture_with(&files, &command, &environment, &unit.cwd).unwrap();
    let captures = unit.captures.parent().unwrap().parent().unwrap();
    files.put(captures.join("runtime-provenance"), serde_json::to_vec(&runtime).unwrap());
    unit.runtime = Some(runtime.root.clone());
    let old_target = unit.target.clone();
    let runtime_artifact = old_target.join("libgamma_rt-runtime.rmeta");
    unit.invocation.externs.push(runtime_artifact.clone());
    assert!(unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_some());
    unit.target = old_target.with_file_name("new-outputs");
    unit.invocation.out_dir = unit.target.clone();
    let old_dep = old_target.join("unit-hash.d");
    let text = String::from_utf8(files.read(&old_dep).unwrap())
        .unwrap()
        .replace(old_target.as_str(), unit.target.as_str());
    files.put(unit.target.join("unit-hash.d"), text);
    files.put(unit.target.join("libunit-hash.rmeta"), b"rust\0new target metadata");
    let (dependency_files, mut dependency) = fixture("dependency");
    files.0.borrow_mut().extend(dependency_files.0.into_inner());
    dependency.target = unit.target.clone();
    dependency.invocation.out_dir = unit.target.clone();
    let text = String::from_utf8(files.read(&old_target.join("dependency-hash.d")).unwrap())
        .unwrap()
        .replace(old_target.as_str(), unit.target.as_str());
    files.put(unit.target.join("dependency-hash.d"), text);
    let dependency_artifact = unit.target.join("libdependency-hash.rmeta");
    files.put(&dependency_artifact, b"rust\0dependency metadata");
    let mut record = dependency.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    files.put(dependency.captures.join("dependency.json"), serde_json::to_vec(&record).unwrap());
    unit.invocation.externs = vec![dependency_artifact];
    assert!(unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_some());
    record.externs.push(runtime_artifact.clone());
    files.put(dependency.captures.join("dependency.json"), serde_json::to_vec(&record).unwrap());
    assert!(runtime.valid(&files, &runtime.root, Some(&unit.compiler)));
    assert!(runtime.contains(&files, &runtime_artifact));
    assert!(unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_none());
}

#[test]
fn runtime_capture_records_exact_source_shape_artifacts_compiler_and_command_context() {
    let (files, unit, command, environment) = runtime_fixture();
    let root = Utf8Path::new(&environment[RUNTIME_VAR]);
    let capture = runtime::capture_with(&files, &command, &environment, &unit.cwd).unwrap();
    assert_eq!(capture.root, root);
    assert_eq!(
        capture.files.iter().map(|(path, _hash)| path.clone()).collect::<Vec<_>>(),
        [root.join("Cargo.toml"), root.join("src").join("lib.rs")]
    );
    assert_eq!(capture.files[1].1, *blake3::hash(b"pub fn selected() -> bool { false }").as_bytes());
    assert_eq!(
        capture.artifacts,
        [
            (
                unit.target.join("libgamma_rt-runtime.rmeta"),
                *blake3::hash(b"rust\0runtime metadata").as_bytes()
            ),
            (
                unit.target.join("libgamma_rt-runtime.rlib"),
                *blake3::hash(b"!<arch>\nruntime archive").as_bytes()
            )
        ]
    );
    assert!(capture.valid(&files, root, Some(&unit.compiler)));
    for emit in ["--emit=dep-info,link", "--emit=dep-info,metadata"] {
        let altered = alter_command(&command, |args| args[2] = emit.into());
        let other = runtime::capture_with(&files, &altered, &environment, &unit.cwd).unwrap();
        assert_eq!(other.files, capture.files);
        assert_eq!(other.artifacts, capture.artifacts);
        assert_ne!(other.context, capture.context, "identity retains the original emit option");
    }
    let mut environment = environment;
    environment.insert(COMMAND_VAR.into(), "another parent command".into());
    assert_ne!(
        runtime::capture_with(&files, &command, &environment, &unit.cwd).unwrap().context,
        capture.context
    );
}

#[test]
fn runtime_compiler_and_crate_guards_are_not_masked_by_missing_artifacts_or_inputs() {
    for boundary in ["relative-compiler", "compiler-name", "crate-name", "manifest-encoding"] {
        let (files, unit, command, environment) = runtime_fixture();
        let mut command = command;
        if boundary == "manifest-encoding" {
            files.put(Utf8Path::new(&environment[RUNTIME_VAR]).join("Cargo.toml"), [0xff]);
        } else {
            let mut args = command.get_args().map(OsString::from).collect::<Vec<_>>();
            let compiler = match boundary {
                "relative-compiler" => Utf8PathBuf::from("rustc"),
                "compiler-name" => unit.compiler.with_file_name("other.exe"),
                _ => {
                    *args.iter_mut().find(|arg| *arg == "--crate-name=gamma_rt").unwrap() = "--crate-name=other".into();
                    files.put(
                        unit.target.join("other-runtime.d"),
                        files.read(&unit.target.join("gamma_rt-runtime.d")).unwrap(),
                    );
                    unit.compiler.clone()
                }
            };
            files.put(&compiler, files.read(&unit.compiler).unwrap());
            command = Command::new(compiler);
            let _command = command.args(args);
        }
        assert!(
            runtime::capture_with(&files, &command, &environment, &unit.cwd).is_none(),
            "{boundary}"
        );
    }
}

#[test]
fn runtime_capture_accepts_nested_outputs_but_not_dep_info_from_outside_the_target() {
    for inside in [false, true] {
        let (files, unit, command, environment) = runtime_fixture();
        let out = if inside {
            unit.target.join("nested")
        } else {
            drive().join("outside-output")
        };
        let dep = files.read(&unit.target.join("gamma_rt-runtime.d")).unwrap();
        let text = if inside {
            for name in ["libgamma_rt-runtime.rmeta", "libgamma_rt-runtime.rlib"] {
                files.put(out.join(name), files.read(&unit.target.join(name)).unwrap());
            }
            String::from_utf8(dep).unwrap().replace(unit.target.as_str(), out.as_str())
        } else {
            String::from_utf8(dep).unwrap()
        };
        files.put(out.join("gamma_rt-runtime.d"), text);
        let command = alter_command(&command, |args| {
            let index = args.iter().position(|arg| arg == "--out-dir").unwrap() + 1;
            args[index] = out.into();
        });
        assert_eq!(runtime::capture_with(&files, &command, &environment, &unit.cwd).is_some(), inside);
    }
}

#[test]
fn native_runtime_capture_declines_unreadable_source_tree_and_source_bytes() {
    use super::snapshot_io::{FileData, Script};

    for directory_failure in [false, true] {
        let (files, unit, command, environment) = runtime_fixture();
        let root = Utf8Path::new(&environment[RUNTIME_VAR]);
        let mut script = Script::from_files(
            files
                .0
                .borrow()
                .iter()
                .map(|(path, bytes)| (path.clone().into_std_path_buf(), bytes.clone())),
        );
        if directory_failure {
            let _previous = script
                .directories
                .insert(root.to_owned().into_std_path_buf(), Err(io::ErrorKind::PermissionDenied));
        } else {
            let mut data = FileData::bytes(b"runtime source");
            data.read_error = Some(io::ErrorKind::PermissionDenied);
            let _previous = script.files.insert(root.join("src").join("lib.rs").into_std_path_buf(), Ok(data));
        }
        snapshot_io::with(script, || {
            assert!(runtime::capture_with(&NativeFiles, &command, &environment, &unit.cwd).is_none());
        });
    }
}

#[test]
fn runtime_capture_declines_every_unowned_or_executable_compilation_context() {
    let (files, unit, command, environment) = runtime_fixture();
    for name in [RUNTIME_VAR, TARGET_VAR, "CARGO_MANIFEST_DIR", COMMAND_VAR] {
        let mut environment = environment.clone();
        let _removed = environment.remove(name);
        assert!(runtime::capture_with(&files, &command, &environment, &unit.cwd).is_none(), "{name}");
    }
    for (name, value) in [
        (RUNTIME_VAR, "relative".to_owned()),
        ("CARGO_MANIFEST_DIR", unit.root.to_string()),
        (TARGET_VAR, unit.root.to_string()),
        ("OUT_DIR", String::new()),
        (ORIGINAL_WRAPPER_VAR, String::new()),
        ("RUSTC_WORKSPACE_WRAPPER", String::new()),
    ] {
        let mut environment = environment.clone();
        environment.insert(name.into(), value);
        assert!(runtime::capture_with(&files, &command, &environment, &unit.cwd).is_none(), "{name}");
    }
    for compiler in [Utf8PathBuf::from("rustc"), unit.compiler.with_file_name("other.exe")] {
        let mut altered = Command::new(compiler);
        let _altered = altered.args(command.get_args());
        assert!(runtime::capture_with(&files, &altered, &environment, &unit.cwd).is_none());
    }
    for case in [
        "crate-name",
        "test",
        "opaque",
        "extern",
        "crate-type",
        "source",
        "unknown-option",
        "missing-args",
    ] {
        let altered = alter_command(&command, |args| match case {
            "crate-name" => args[0] = "--crate-name=not_gamma_rt".into(),
            "test" => args.push("--test".into()),
            "opaque" => args.extend([OsString::from("--extern"), OsString::from("opaque")]),
            "extern" => args.extend([OsString::from("--extern"), format!("dep={}", unit.target.join("dep.rmeta")).into()]),
            "crate-type" => args[1] = "--crate-type=lib,proc-macro".into(),
            "source" => *args.last_mut().unwrap() = unit.invocation.source.clone().into(),
            "unknown-option" => args.push("--sysroot=other".into()),
            _ => args.clear(),
        });
        assert!(runtime::capture_with(&files, &altered, &environment, &unit.cwd).is_none(), "{case}");
    }
    let altered = alter_command(&command, |args| args[1] = "--crate-type=rlib".into());
    assert!(runtime::capture_with(&files, &altered, &environment, &unit.cwd).is_some());
}

#[test]
fn runtime_manifest_closure_checks_nested_dependency_sections_and_build_switches() {
    for manifest in [
        "[package]\nname='cargo-gamma-rt'\nbuild=false\n[lib]\nproc-macro=false\n[dependencies]\n[dev-dependencies]\n[build-dependencies]\n",
        "[package]\nname='cargo-gamma-rt'\n[target.'cfg(unix)'.dependencies]\n",
        "[package]\nname='cargo-gamma-rt'\n[metadata]\nlabels=['one','two']\n",
    ] {
        let (files, unit, command, environment) = runtime_fixture();
        files.put(Utf8Path::new(&environment[RUNTIME_VAR]).join("Cargo.toml"), manifest);
        assert!(
            runtime::capture_with(&files, &command, &environment, &unit.cwd).is_some(),
            "{manifest}"
        );
    }
    for manifest in [
        "[package]\nname='other'\n",
        "[package]\nname=3\n",
        "[package]\nversion='1'\n",
        "name='cargo-gamma-rt'\n",
        "not TOML [",
        "[package]\nname='cargo-gamma-rt'\nbuild=true\n",
        "[package]\nname='cargo-gamma-rt'\nbuild='build.rs'\n",
        "[package]\nname='cargo-gamma-rt'\n[lib]\nproc-macro=true\n",
        "[package]\nname='cargo-gamma-rt'\n[dependencies]\nx='1'\n",
        "[package]\nname='cargo-gamma-rt'\n[dev-dependencies]\nx='1'\n",
        "[package]\nname='cargo-gamma-rt'\n[build-dependencies]\nx='1'\n",
        "[package]\nname='cargo-gamma-rt'\n[target.'cfg(unix)'.dependencies]\nx='1'\n",
        "[package]\nname='cargo-gamma-rt'\ndependencies=[]\n",
        "[package]\nname='cargo-gamma-rt'\n[[metadata.items]]\nbuild=true\n",
        "[package]\nname='cargo-gamma-rt'\n[[metadata.items]]\nbuild=false\n[[metadata.items]]\nbuild=true\n",
        "[package]\nname='cargo-gamma-rt'\n[[metadata.items]]\n[metadata.items.dependencies]\nx='1'\n",
    ] {
        let (files, unit, command, environment) = runtime_fixture();
        files.put(Utf8Path::new(&environment[RUNTIME_VAR]).join("Cargo.toml"), manifest);
        assert!(
            runtime::capture_with(&files, &command, &environment, &unit.cwd).is_none(),
            "{manifest}"
        );
    }
}

#[test]
fn runtime_artifact_capture_requires_named_readable_outputs_under_target() {
    for case in [
        "no-artifacts",
        "missing-dep",
        "non-utf8-dep",
        "multiple-outputs",
        "outside-target",
        "wrong-name",
        "missing-artifact",
        "missing-compiler",
        "missing-source",
    ] {
        let (files, unit, command, environment) = runtime_fixture();
        let root = Utf8Path::new(&environment[RUNTIME_VAR]);
        let dep = unit.target.join("gamma_rt-runtime.d");
        match case {
            "no-artifacts" => files.put(&dep, "phony:\nignored\n"),
            "missing-dep" => {
                let _removed = files.0.borrow_mut().remove(&dep);
            }
            "non-utf8-dep" => files.put(&dep, [0xff]),
            "multiple-outputs" => files.put(
                &dep,
                format!(
                    "{} {}: {}\n",
                    unit.target.join("libgamma_rt-runtime.rmeta"),
                    unit.target.join("libgamma_rt-runtime.rlib"),
                    root.join("src").join("lib.rs")
                ),
            ),
            "outside-target" | "wrong-name" => {
                let artifact = if case == "outside-target" {
                    root.join("libgamma_rt-runtime.rmeta")
                } else {
                    unit.target.join("libwrong-runtime.rmeta")
                };
                files.put(&artifact, "artifact");
                files.put(&dep, format!("{artifact}: {}\n", root.join("src").join("lib.rs")));
            }
            "missing-artifact" => {
                let _removed = files.0.borrow_mut().remove(&unit.target.join("libgamma_rt-runtime.rmeta"));
            }
            "missing-compiler" => {
                let _removed = files.0.borrow_mut().remove(&unit.compiler);
            }
            _ => {
                let _removed = files.0.borrow_mut().remove(&root.join("Cargo.toml"));
            }
        }
        assert!(runtime::capture_with(&files, &command, &environment, &unit.cwd).is_none(), "{case}");
    }
}

#[test]
fn runtime_validity_and_membership_distinguish_shape_sources_compiler_and_artifact_bytes() {
    for case in [
        "unchanged",
        "root",
        "compiler-path",
        "compiler-bytes",
        "source-bytes",
        "source-shape",
        "empty-stamps",
        "artifact-bytes",
        "other-artifact",
    ] {
        let (files, unit, command, environment) = runtime_fixture();
        let root = Utf8Path::new(&environment[RUNTIME_VAR]);
        let mut runtime = runtime::capture_with(&files, &command, &environment, &unit.cwd).unwrap();
        let mut expected_root = root.to_owned();
        let mut compiler = unit.compiler.clone();
        let artifact = unit.target.join("libgamma_rt-runtime.rmeta");
        let mut expected_artifact = artifact.clone();
        match case {
            "unchanged" => {}
            "root" => expected_root = unit.root.clone(),
            "compiler-path" => compiler = unit.compiler.with_file_name("other.exe"),
            "compiler-bytes" => files.put(&compiler, "other compiler"),
            "source-bytes" => files.put(root.join("src").join("lib.rs"), "fn changed() {}"),
            "source-shape" => files.put(root.join("src").join("added.rs"), "fn added() {}"),
            "empty-stamps" => runtime.files.clear(),
            "artifact-bytes" => files.put(&artifact, "changed artifact"),
            _ => {
                expected_artifact = unit.target.join("other.rmeta");
                files.put(&expected_artifact, files.read(&artifact).unwrap());
            }
        }
        assert_eq!(
            runtime.valid(&files, &expected_root, Some(&compiler)),
            matches!(case, "unchanged" | "artifact-bytes" | "other-artifact"),
            "{case}"
        );
        assert_eq!(
            runtime.contains(&files, &expected_artifact),
            !matches!(case, "artifact-bytes" | "other-artifact"),
            "{case}"
        );
        if case == "compiler-path" {
            assert!(
                runtime.valid(&files, root, None),
                "proof context may omit the caller's compiler path but still hashes the captured compiler"
            );
        }
    }
}
