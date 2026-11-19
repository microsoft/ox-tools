// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt::Write as _;

use super::*;

fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[test]
fn metadata_rejections_reach_the_selected_option_or_single_input_boundary() {
    for extra in [
        arguments(&["-C", "unknown=value"]),
        arguments(&["-C", "=value"]),
        arguments(&["-L", "native=one"]),
        arguments(&["-L", "dependency"]),
        arguments(&["--target", "target.json"]),
    ] {
        let mut args = arguments(&["unit.rs", "--emit=dep-info,metadata"]);
        args.extend(extra);
        assert_eq!(metadata_arguments(&args), None, "{args:?}");
    }
}

#[test]
fn single_input_metadata_replay_requires_a_rust_source() {
    for source in ["input.txt", "unit.rmeta", "unit", ""] {
        assert_eq!(
            metadata_arguments(&arguments(&[source, "--emit=dep-info,metadata"])),
            None,
            "{source:?}"
        );
    }
    assert_eq!(
        metadata_arguments(&arguments(&["unit.rs", "--emit=dep-info,metadata"])),
        Some(Vec::new())
    );
}

#[test]
fn relative_compilers_remain_open_even_with_complete_toolchain_files() {
    let (files, mut unit) = fixture("unit");
    let toolchain = Utf8Path::new("relative-toolchain");
    unit.compiler = toolchain.join("bin").join("rustc.exe");
    files.put(&unit.compiler, b"compiler");
    files.put(toolchain.join("lib").join("libstd.rlib"), b"standard library");
    assert_eq!(digest(&files, &unit.compiler), Some(blake3::hash(b"compiler").into()));
    assert!(unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_none());
}

#[test]
fn native_snapshot_adapter_preserves_bytes_limits_and_each_io_failure() {
    use super::snapshot_io::{FileData, Script};

    let path = drive().join("scripted-snapshot");
    let bytes = b"exact compiler input";
    let mut script = Script::default();
    let _previous = script.files.insert(path.clone().into_std_path_buf(), Ok(FileData::bytes(bytes)));
    snapshot_io::with(script, || {
        assert_eq!(read_snapshot(&path).unwrap(), bytes);
        assert_eq!(NativeFiles.read(&path), Some(bytes.to_vec()));
    });
    for case in ["missing", "open", "metadata", "read", "oversized"] {
        let mut script = Script::default();
        let mut data = FileData::bytes(bytes);
        let result = match case {
            "missing" => Err(io::ErrorKind::NotFound),
            "open" => Err(io::ErrorKind::PermissionDenied),
            "metadata" => {
                data.length = Err(io::ErrorKind::PermissionDenied);
                Ok(data)
            }
            "read" => {
                data.read_error = Some(io::ErrorKind::PermissionDenied);
                Ok(data)
            }
            _ => {
                data.length = Ok(MAX_FILE_BYTES + 1);
                Ok(data)
            }
        };
        let _previous = script.files.insert(path.clone().into_std_path_buf(), result);
        snapshot_io::with(script, || {
            let cause = read_snapshot(&path).expect_err("the adapter cannot capture unavailable or oversized input");
            let expected = match case {
                "missing" => io::ErrorKind::NotFound,
                "oversized" => io::ErrorKind::Other,
                _ => io::ErrorKind::PermissionDenied,
            };
            assert_eq!(cause.kind(), expected, "{case}");
            assert_eq!(NativeFiles.read(&path), None, "{case}");
        });
    }
    let mut script = Script::default();
    let mut data = FileData::bytes(bytes);
    data.length = Ok(MAX_FILE_BYTES);
    let _previous = script.files.insert(path.clone().into_std_path_buf(), Ok(data));
    snapshot_io::with(script, || {
        assert_eq!(
            read_snapshot(&path).unwrap(),
            bytes,
            "shrinking after metadata does not exceed the capture budget"
        );
    });
}

#[test]
fn native_directory_adapter_walks_regular_files_and_declines_unreadable_entry_kinds() {
    use super::snapshot_io::{Kind, Script};

    let root = drive().join("scripted-tree");
    let target = root.join("target");
    let paths = [root.join("a.rs"), root.join("nested").join("b.rs")];
    let make = || Script::from_files(paths.iter().map(|path| (path.clone().into_std_path_buf(), b"input".to_vec())));
    let mut script = make();
    script
        .directories
        .get_mut(root.as_std_path())
        .unwrap()
        .as_mut()
        .unwrap()
        .push(Ok((target.clone().into_std_path_buf(), Err(io::ErrorKind::PermissionDenied))));
    snapshot_io::with(script, || {
        assert_eq!(snapshot_paths(&root, &target), Some(paths.to_vec()));
        assert_eq!(NativeFiles.files(&root, &target), Some(paths.to_vec()));
    });
    for kind in [Ok(Kind::Symlink), Ok(Kind::Other), Err(io::ErrorKind::PermissionDenied)] {
        let mut script = make();
        let _previous = script.directories.insert(
            root.clone().into_std_path_buf(),
            Ok(vec![Ok((paths[0].clone().into_std_path_buf(), kind))]),
        );
        snapshot_io::with(script, || assert_eq!(snapshot_paths(&root, &target), None));
    }
    for entries in [Err(io::ErrorKind::PermissionDenied), Ok(vec![Err(io::ErrorKind::PermissionDenied)])] {
        let mut script = make();
        let _previous = script.directories.insert(root.clone().into_std_path_buf(), entries);
        snapshot_io::with(script, || assert_eq!(snapshot_paths(&root, &target), None));
    }
    for ancestor in root.ancestors() {
        for result in [Ok(true), Err(io::ErrorKind::PermissionDenied)] {
            let mut script = make();
            let _previous = script.symlinks.insert(ancestor.to_owned().into_std_path_buf(), result);
            snapshot_io::with(script, || {
                assert!(!unambiguous_path(&root), "{ancestor}");
                assert_eq!(snapshot_paths(&root, &target), None, "{ancestor}");
            });
        }
    }
}

#[test]
fn native_directory_adapter_declines_non_utf8_entry_paths() {
    use super::snapshot_io::{Kind, Script};

    #[cfg(windows)]
    let name = {
        use std::os::windows::ffi::OsStringExt as _;
        OsString::from_wide(&[0xd800])
    };
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStringExt as _;
        OsString::from_vec(vec![0xff])
    };
    let root = drive().join("scripted-native-name");
    let mut script = Script::default();
    let _previous = script.directories.insert(
        root.clone().into_std_path_buf(),
        Ok(vec![Ok((root.as_std_path().join(name), Ok(Kind::File)))]),
    );
    snapshot_io::with(script, || assert_eq!(snapshot_paths(&root, Utf8Path::new("")), None));
}

#[test]
fn native_inventory_tracks_paths_and_declines_directory_read_failures() {
    use super::snapshot_io::Script;

    let root = drive().join("native-inventory");
    let source = root.join("lib.rs");
    let make = || Script::from_files(core::iter::once((source.clone().into_std_path_buf(), b"original".to_vec())));
    snapshot_io::with(make(), || {
        let first = inventory(&NativeFiles, &root, Utf8Path::new("")).unwrap();
        snapshot_io::write(&source, b"changed").unwrap();
        assert_eq!(inventory(&NativeFiles, &root, Utf8Path::new("")), Some(first));
        snapshot_io::write(root.join("added.rs"), b"new source").unwrap();
        assert_ne!(inventory(&NativeFiles, &root, Utf8Path::new("")), Some(first));
    });
    let mut script = make();
    let _previous = script
        .directories
        .insert(root.clone().into_std_path_buf(), Err(io::ErrorKind::PermissionDenied));
    snapshot_io::with(script, || assert_eq!(inventory(&NativeFiles, &root, Utf8Path::new("")), None));
}

#[test]
fn native_source_context_adapter_accepts_only_complete_current_capture_sets() {
    use super::snapshot_io::Script;

    let (files, unit) = fixture("unit");
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    let capture = unit.captures.join("unit.json");
    files.put(&capture, serde_json::to_vec(&record).unwrap());
    let stdout = serde_json::json!({
        "reason": "compiler-artifact",
        "filenames": [unit.target.join("libunit-hash.rmeta")],
    })
    .to_string();
    let make = || {
        Script::from_files(
            files
                .0
                .borrow()
                .iter()
                .map(|(path, bytes)| (path.clone().into_std_path_buf(), bytes.clone())),
        )
    };
    snapshot_io::with(make(), || {
        let context = source_only_context(&unit.captures, &stdout, &unit.root, &unit.target, &drive().join("runtime")).unwrap();
        assert_eq!(context.units, vec![unit.context]);
        assert!(context.toolchain.iter().any(|(path, _)| path == &unit.compiler));
    });
    for unreadable in [&capture, &unit.compiler, &unit.invocation.source] {
        let mut script = make();
        let _previous = script
            .files
            .insert(unreadable.clone().into_std_path_buf(), Err(io::ErrorKind::PermissionDenied));
        snapshot_io::with(script, || {
            assert!(
                source_only_context(&unit.captures, &stdout, &unit.root, &unit.target, &drive().join("runtime")).is_none(),
                "{unreadable}"
            );
        });
    }
}

#[test]
fn metadata_option_matrix_preserves_searches_and_rejects_open_inputs() {
    let base = arguments(&["unit.rs", "--emit=dep-info,metadata"]);
    assert_eq!(metadata_arguments(&base), Some(Vec::new()));
    for name in [
        "--crate-name",
        "--crate-type",
        "--edition",
        "--error-format",
        "--json",
        "--diagnostic-width",
        "--cfg",
        "--check-cfg",
        "--cap-lints",
        "--extern",
        "--out-dir",
        "--allow",
        "--warn",
        "--deny",
        "--forbid",
        "--force-warn",
    ] {
        for extra in [arguments(&[name, "value"]), vec![format!("{name}=value").into()]] {
            let mut args = base.clone();
            args.extend(extra);
            assert_eq!(metadata_arguments(&args), Some(Vec::new()), "{name}");
        }
        let mut args = base.clone();
        args.push(name.into());
        assert!(metadata_arguments(&args).is_none(), "missing {name} value");
    }
    for name in [
        "metadata",
        "extra-filename",
        "opt-level",
        "embed-bitcode",
        "debuginfo",
        "incremental",
        "strip",
        "debug-assertions",
        "overflow-checks",
        "panic",
        "codegen-units",
    ] {
        for extra in [
            vec![OsString::from("-C"), format!("{name}=value").into()],
            vec![format!("-C{name}=value").into()],
        ] {
            let mut args = base.clone();
            args.extend(extra);
            assert!(metadata_arguments(&args).is_some(), "{name}");
        }
        for option in [name.to_owned(), format!("{name}=")] {
            for extra in [arguments(&["-C", &option]), vec![format!("-C{option}").into()]] {
                let mut args = base.clone();
                args.extend(extra);
                assert_eq!(metadata_arguments(&args), None, "{option}");
            }
        }
    }
}

#[test]
fn metadata_target_and_input_matrix_preserves_searches_and_rejects_open_inputs() {
    let base = arguments(&["unit.rs", "--emit=dep-info,metadata"]);
    for target in ["aarch64-unknown-linux-gnu", "aarch64-pc-windows-msvc", "A9-b"] {
        for extra in [arguments(&["--target", target]), vec![format!("--target={target}").into()]] {
            let mut args = base.clone();
            args.extend(extra);
            assert!(metadata_arguments(&args).is_some(), "{target}");
        }
    }
    for target in ["", "target.json", "custom/target", "a_b", "x86_64-unknown-linux-gnu", "a b", "é"] {
        for extra in [arguments(&["--target", target]), vec![format!("--target={target}").into()]] {
            let mut args = base.clone();
            args.extend(extra);
            assert_eq!(metadata_arguments(&args), None, "{target}");
        }
    }
    let mut args = base.clone();
    args.extend(arguments(&["--test", "-L", "dependency=one", "-Ldependency=two"]));
    assert_eq!(metadata_arguments(&args), Some(vec!["one".into(), "two".into()]));
    for extra in [
        "-L",
        "-Lnative=one",
        "-Ldependency",
        "--target",
        "-C",
        "-Cunknown=value",
        "-C=value",
        "-Cxyzzy=value",
        "--emit=metadata",
        "--emit=dep-info,link",
        "--sysroot=custom",
        "@args",
        "other.rs",
        "input.txt",
        "--test=value",
        "--unknown",
        "--allowance=value",
    ] {
        let mut args = base.clone();
        args.push(extra.into());
        assert!(metadata_arguments(&args).is_none(), "{extra}");
    }
    for values in [
        vec!["unit.rs"],
        vec!["--emit=dep-info,metadata"],
        vec!["-unit.rs", "--emit=dep-info,metadata"],
    ] {
        assert!(metadata_arguments(&arguments(&values)).is_none());
    }
}

#[test]
fn eligibility_checks_every_opaque_unit_kind_and_environment_trigger() {
    let (_, original) = fixture("unit");
    let args = arguments(&["unit.rs", "--emit=dep-info,metadata"]);
    assert!(eligible(&args, &original.invocation, &BTreeMap::new()));
    for kind in ["rlib", "rmeta"] {
        let mut invocation = original.invocation.clone();
        invocation.externs.push(original.target.join(format!("libdependency.{kind}")));
        assert!(eligible(&args, &invocation, &BTreeMap::new()));
    }
    for case in ["opaque", "proc-macro", "build-script", "dll", "no-extension"] {
        let mut invocation = original.invocation.clone();
        match case {
            "opaque" => invocation.opaque_extern = true,
            "proc-macro" => invocation.crate_types.push("proc-macro".into()),
            "build-script" => invocation.crate_name = "build_script_build".into(),
            "dll" => invocation.externs.push("plugin.dll".into()),
            _ => invocation.externs.push("plugin".into()),
        }
        assert!(!eligible(&args, &invocation, &BTreeMap::new()), "{case}");
    }
    for name in ["OUT_DIR", ORIGINAL_WRAPPER_VAR, "RUSTC_WORKSPACE_WRAPPER", "CARGO_BIN_EXE_tool"] {
        assert!(
            !eligible(&args, &original.invocation, &BTreeMap::from([(name.into(), String::new())])),
            "{name}"
        );
    }
    assert!(eligible(
        &args,
        &original.invocation,
        &BTreeMap::from([("MY_OUT_DIR".into(), "unused".into()), ("CARGO_BIN_EXE".into(), "unused".into()),])
    ));
}

#[test]
fn prepared_context_uses_explicit_cwd_generation_arguments_and_environment() {
    let (_, unit) = fixture("unit");
    let generation = "aB09".repeat(16);
    let mut environment = BTreeMap::from([
        (REPLAY_VAR.into(), generation.clone()),
        (ROOT_VAR.into(), unit.root.to_string()),
        (TARGET_VAR.into(), unit.target.to_string()),
        (CAPTURE_DIR_VAR.into(), unit.target.join("captures").to_string()),
        (RUNTIME_VAR.into(), drive().join("runtime").to_string()),
    ]);
    let make_command = |out: &Utf8Path| {
        let mut command = Command::new(&unit.compiler);
        let _command = command.args([
            OsString::from("--crate-name=unit"),
            OsString::from("--crate-type=lib"),
            OsString::from("--emit=dep-info,metadata"),
            OsString::from("--out-dir"),
            out.into(),
            OsString::from("-Ldependency=search"),
            OsString::from("unit.rs"),
        ]);
        command
    };
    let command = make_command(&unit.target);
    let (directory, prepared) = prepare_with(&command, &environment, &unit.root).unwrap();
    assert_eq!(directory, unit.target.join("captures").join("replay").join(&generation));
    assert_eq!(prepared.captures, directory);
    assert_eq!(prepared.cwd, unit.root);
    assert_eq!(prepared.root, unit.root);
    assert_eq!(prepared.target, unit.target);
    assert_eq!(prepared.compiler, unit.compiler);
    assert_eq!(prepared.invocation.source, Utf8Path::new("unit.rs"));
    assert_eq!(prepared.searches, vec![Utf8PathBuf::from("search")]);
    assert_eq!(prepared.runtime, Some(drive().join("runtime")));
    let mut identity = make_command(&unit.target);
    let _identity = identity.current_dir(&unit.root);
    assert_eq!(
        Some(prepared.context),
        command_fingerprint(&identity, environment.iter().map(|(k, v)| (k.into(), v.into())))
    );
    let _removed = environment.remove(RUNTIME_VAR);
    assert!(prepare_with(&command, &environment, &unit.root).unwrap().1.runtime.is_none());
    for name in [REPLAY_VAR, ROOT_VAR, TARGET_VAR, CAPTURE_DIR_VAR] {
        let mut environment = environment.clone();
        let _removed = environment.remove(name);
        assert!(prepare_with(&command, &environment, &unit.root).is_none(), "{name}");
    }
    for invalid in [
        String::new(),
        "a".repeat(63),
        "a".repeat(65),
        "z".repeat(64),
        format!("{}z", "a".repeat(63)),
    ] {
        let mut environment = environment.clone();
        environment.insert(REPLAY_VAR.into(), invalid.clone());
        assert!(prepare_with(&command, &environment, &unit.root).is_none(), "{invalid}");
    }
    for (name, value) in [
        (ROOT_VAR, "relative".to_owned()),
        (TARGET_VAR, "relative".to_owned()),
        (TARGET_VAR, unit.root.to_string()),
        ("OUT_DIR", "opaque".to_owned()),
    ] {
        let mut environment = environment.clone();
        environment.insert(name.into(), value);
        assert!(prepare_with(&command, &environment, &unit.root).is_none(), "{name}");
    }
    assert!(prepare_with(&command, &environment, &drive().join("elsewhere")).is_none());
    assert!(prepare_with(&make_command(&unit.root), &environment, &unit.root).is_none());
    assert!(prepare_with(&Command::new(&unit.compiler), &environment, &unit.root).is_none());
}

#[test]
fn fingerprint_applies_environment_overrides_removals_order_and_utf8_boundaries() {
    let make = || {
        let mut command = Command::new("compiler");
        let _command = command.current_dir("workspace").args(["ab", "c"]);
        command
    };
    let inherited = || {
        [
            (OsString::from("A"), OsString::from("old")),
            (OsString::from("B"), OsString::from("keep")),
        ]
    };
    let mut command = make();
    let _command = command.env("A", "new").env_remove("B").env("C", "added");
    let expected = command_fingerprint(&make(), [("C".into(), "added".into()), ("A".into(), "new".into())]);
    assert!(expected.is_some());
    assert_eq!(command_fingerprint(&command, inherited()), expected);
    assert_ne!(expected, command_fingerprint(&make(), inherited()));
    let mut reordered = make();
    let _reordered = reordered.env("C", "added").env_remove("absent").env("A", "new").env_remove("B");
    assert_eq!(command_fingerprint(&reordered, inherited()), expected);
    let mut segmented = Command::new("compiler");
    let _segmented = segmented.current_dir("workspace").args(["a", "bc"]);
    assert_ne!(command_fingerprint(&make(), []), command_fingerprint(&segmented, []));
    assert_ne!(
        command_fingerprint(&make(), [("ab".into(), "c".into())]),
        command_fingerprint(&make(), [("a".into(), "bc".into())])
    );
    assert_ne!(
        command_fingerprint(&make(), [("FIRST".into(), "unchanged".into())]),
        command_fingerprint(&make(), [("SECOND".into(), "unchanged".into())]),
        "renaming an environment key changes the compiler context even when its value stays equal"
    );
    assert!(command_fingerprint(&Command::new("compiler"), []).is_none());
}

#[test]
#[cfg(any(unix, windows))]
fn non_utf8_command_fields_and_environment_decline_capture_instead_of_panicking() {
    fn invalid() -> OsString {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt as _;
            OsString::from_wide(&[0xd800])
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            OsString::from_vec(vec![0xff])
        }
    }
    for field in [
        "compiler",
        "cwd",
        "argument",
        "inherited-name",
        "inherited-value",
        "override-name",
        "override-value",
        "removed-name",
    ] {
        let mut command = Command::new(if field == "compiler" { invalid() } else { "compiler".into() });
        let _command = command.current_dir("workspace").args(["unit.rs", "--emit=dep-info,metadata"]);
        let mut environment = Vec::new();
        match field {
            "compiler" => {}
            "cwd" => {
                let _command = command.current_dir(invalid());
            }
            "argument" => {
                let _command = command.arg(invalid());
            }
            "inherited-name" => environment.push((invalid(), "value".into())),
            "inherited-value" => environment.push(("name".into(), invalid())),
            "override-name" => {
                let _command = command.env(invalid(), "value");
            }
            "override-value" => {
                let _command = command.env("name", invalid());
            }
            _ => {
                let _command = command.env_remove(invalid());
            }
        }
        assert!(command_fingerprint(&command, environment).is_none(), "{field}");
    }
    let mut args = arguments(&["unit.rs", "--emit=dep-info,metadata"]);
    args.push(invalid());
    assert!(metadata_arguments(&args).is_none());
}

#[test]
fn bounded_snapshot_reader_handles_exact_limits_growth_and_io_errors() {
    struct NeverRead;
    impl io::Read for NeverRead {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            panic!("oversized metadata is rejected before reading");
        }
    }
    struct FailedRead;
    impl io::Read for FailedRead {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "scripted read failure"))
        }
    }
    for (length, bytes, limit, expected) in [
        (0, b"".as_slice(), 0, b"".as_slice()),
        (3, b"abc".as_slice(), 3, b"abc".as_slice()),
        (3, b"a".as_slice(), 3, b"a".as_slice()),
    ] {
        assert_eq!(read_snapshot_with(bytes, length, limit).unwrap(), expected);
    }
    assert_eq!(
        read_snapshot_with(NeverRead, 4, 3).unwrap_err().to_string(),
        "compiler snapshot file exceeds its capture limit"
    );
    let mut reader = io::Cursor::new(b"abcdefgh");
    assert_eq!(
        read_snapshot_with(&mut reader, 3, 3).unwrap_err().to_string(),
        "compiler snapshot file grew beyond its capture limit"
    );
    assert_eq!(reader.position(), 4, "growth detection reads only one byte past the budget");
    assert_eq!(
        read_snapshot_with(FailedRead, 1, 3).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn dependency_rules_resolve_relative_paths_deduplicate_and_require_closed_outputs() {
    let (_, mut unit) = fixture("unit");
    unit.invocation.source = "unit.rs".into();
    unit.target = unit.root.join("target");
    let output = unit.target.join("libunit.rmeta");
    let text = format!("unit.rs:\n# env-dep:KEY=value\n{output}: unit.rs included.txt unit.rs\n{output}: included.txt\n\n");
    assert_eq!(
        dep_paths(&text, &unit.invocation, &unit.cwd, &unit.root, &unit.target),
        Some((
            vec![unit.root.join("included.txt"), unit.root.join("unit.rs")],
            vec![output.clone()]
        )),
    );
    let dep = unit.target.join("libunit.d");
    assert_eq!(
        dep_paths(
            &format!("{output}: unit.rs\n{dep}: unit.rs\n{output}: unit.rs"),
            &unit.invocation,
            &unit.cwd,
            &unit.root,
            &unit.target,
        ),
        Some((vec![unit.root.join("unit.rs")], vec![dep, output.clone()])),
        "a closed dep-info output alongside metadata remains reusable"
    );
    assert_eq!(absolute(Utf8Path::new("target").join("libunit.rmeta").as_path(), &unit.cwd), output);
    for text in [
        format!("{output}: included.txt"),
        format!("{}: unit.rs", unit.target.join("unit.o")),
        format!("{}: unit.rs", unit.root.join("outside.rmeta")),
        format!("{output}: {}", drive().join("elsewhere").join("unit.rs")),
        format!("{output}: unit.rs ..{}escape.rs", std::path::MAIN_SEPARATOR),
        format!("{}: unit.rs", unit.target.join("..").join("escape.rmeta")),
        format!("{output} second.rmeta: unit.rs"),
        format!("{output}: unit.rs\nmalformed"),
        "unit.rs:\n# env-dep:X=y".into(),
    ] {
        assert!(
            dep_paths(&text, &unit.invocation, &unit.cwd, &unit.root, &unit.target).is_none(),
            "{text}"
        );
    }
}

#[test]
fn source_closure_distinguishes_nested_extensions_from_ordinary_tokens() {
    for source in [
        "fn ordinary() { let feature = 1; }",
        "const TEXT: &str = \"extern crate hidden; rustc_private feature(foo)\";",
        "macro_rules! safe { () => { fn ordinary() {} } }",
        "#[cfg(feature = \"safe\")] fn ordinary() {}",
    ] {
        assert!(source_tokens_closed(source.parse().unwrap()), "{source}");
    }
    for source in [
        "fn feature() {}",
        "mod nested { extern crate hidden; }",
        "macro_rules! hidden { () => { #[rustc_private] fn item() {} } }",
        "#[cfg_attr(feature = \"nightly\", feature(unstable))] fn item() {}",
    ] {
        assert!(!source_tokens_closed(source.parse().unwrap()), "{source}");
    }
}

#[test]
fn snapshots_reject_incomplete_inputs_toolchains_searches_and_invalid_dep_info() {
    for case in [
        "missing-dep",
        "non-utf8-dep",
        "non-utf8-input",
        "invalid-tokens",
        "missing-source",
        "missing-include",
        "missing-compiler",
        "empty-libraries",
        "wrong-bin",
        "wrong-compiler",
        "relative-compiler",
        "outside-search",
        "parent-search",
    ] {
        let (files, mut unit) = fixture("unit");
        match case {
            "missing-dep" => {
                let _removed = files.0.borrow_mut().remove(&unit.target.join("unit-hash.d"));
            }
            "non-utf8-dep" => files.put(unit.target.join("unit-hash.d"), [0xff]),
            "non-utf8-input" => files.put(&unit.invocation.source, [0xff]),
            "invalid-tokens" => files.put(&unit.invocation.source, "{"),
            "missing-source" => {
                let _removed = files.0.borrow_mut().remove(&unit.invocation.source);
            }
            "missing-include" => {
                let _removed = files.0.borrow_mut().remove(&unit.root.join("included.txt"));
            }
            "missing-compiler" => {
                let _removed = files.0.borrow_mut().remove(&unit.compiler);
            }
            "empty-libraries" => files
                .0
                .borrow_mut()
                .retain(|path, _| !path.starts_with(drive().join("toolchain").join("lib"))),
            "wrong-bin" => {
                unit.compiler = drive().join("toolchain").join("other").join("rustc.exe");
                files.put(&unit.compiler, "compiler");
            }
            "wrong-compiler" => {
                unit.compiler = unit.compiler.with_file_name("other.exe");
                files.put(&unit.compiler, "compiler");
            }
            "relative-compiler" => {
                unit.compiler = "toolchain\\bin\\rustc.exe".into();
                files.put(&unit.compiler, "compiler");
            }
            "outside-search" => unit.searches.push(unit.root.clone()),
            _ => unit.searches.push(unit.target.join("..").join("elsewhere")),
        }
        assert!(unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_none(), "{case}");
    }
}

#[test]
fn execution_distinguishes_live_forwarding_status_capture_and_context_misses() {
    for (code, capturable, forwarded) in [
        (Some(0), true, false),
        (Some(0), true, true),
        (Some(0), false, false),
        (Some(1), true, false),
        (None, true, false),
    ] {
        let (files, unit) = fixture("unit");
        let published = RefCell::new(None);
        let relayed = RefCell::new(Vec::new());
        let mut output = success();
        output.code = code;
        output.capturable = capturable;
        output.forwarded = forwarded;
        assert_eq!(
            execute(
                &unit.borrowed(),
                &files,
                None,
                || Ok(output),
                |record| *published.borrow_mut() = Some(record.clone()),
                |out, err| {
                    relayed.borrow_mut().push((out.to_vec(), err.to_vec()));
                    Ok(())
                },
            )
            .unwrap(),
            code
        );
        let expected_streams = if forwarded {
            Vec::new()
        } else {
            vec![(success().stdout, success().stderr)]
        };
        assert_eq!(*relayed.borrow(), expected_streams);
        assert_eq!(published.borrow().is_some(), code == Some(0) && capturable);
        if let Some(record) = published.borrow().as_ref() {
            assert_eq!(record.stdout, success().stdout);
            assert_eq!(record.stderr, success().stderr);
            let mut changed = record.clone();
            changed.context[0] ^= 1;
            let ran = Cell::new(false);
            let _code = execute(
                &unit.borrowed(),
                &files,
                Some(&changed),
                || {
                    ran.set(true);
                    Ok(success())
                },
                |_| {},
                |_, _| Ok(()),
            )
            .unwrap();
            assert!(ran.get(), "a changed command context never replays old diagnostics");
        }
    }
    let (files, unit) = fixture("unit");
    assert_eq!(
        execute(
            &unit.borrowed(),
            &files,
            None,
            || Err(io::Error::other("compiler start failed")),
            |_| panic!("failed start cannot publish"),
            |_, _| panic!("failed start has no streams"),
        )
        .unwrap_err()
        .to_string(),
        "compiler start failed"
    );
    assert_eq!(
        execute(
            &unit.borrowed(),
            &files,
            None,
            || Ok(success()),
            |_| panic!("failed relay cannot publish"),
            |_, _| Err(io::Error::other("relay failed")),
        )
        .unwrap_err()
        .to_string(),
        "relay failed"
    );
}

#[test]
fn compilation_can_refresh_output_bytes_but_not_the_output_graph_or_inputs() {
    for case in ["output-bytes", "new-output", "toolchain", "dep-graph", "search-shape"] {
        let (files, mut unit) = fixture("unit");
        if case != "new-output" {
            unit.searches.push(unit.target.clone());
        }
        let published = RefCell::new(None);
        let _code = execute(
            &unit.borrowed(),
            &files,
            None,
            || {
                match case {
                    "output-bytes" => files.put(unit.target.join("libunit-hash.rmeta"), "updated metadata"),
                    "new-output" => {
                        let output = unit.target.join("unit.o");
                        files.put(&output, "object");
                        let mut dep = String::from_utf8(files.read(&unit.target.join("unit-hash.d")).unwrap()).unwrap();
                        writeln!(dep, "{output}: {}", unit.invocation.source).expect("writing to a String is infallible");
                        files.put(unit.target.join("unit-hash.d"), dep);
                    }
                    "toolchain" => files.put(&unit.compiler, "changed compiler"),
                    "dep-graph" => files.put(
                        unit.target.join("unit-hash.d"),
                        format!("{}: {}\n", unit.target.join("libunit-hash.rmeta"), unit.invocation.source),
                    ),
                    _ => files.put(unit.target.join("new-candidate.rmeta"), "new candidate"),
                }
                Ok(success())
            },
            |record| *published.borrow_mut() = Some(record.clone()),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(published.borrow().is_some(), case == "output-bytes", "{case}");
        if let Some(record) = published.borrow().as_ref() {
            assert!(unit.borrowed().reusable(&files, record), "the new output stamps are captured");
        }
    }
}

#[test]
fn stream_reader_retries_interruptions_and_keeps_forwarding_after_overflow() {
    struct ScriptedReader {
        steps: std::collections::VecDeque<io::Result<Vec<u8>>>,
        reads: usize,
    }
    impl io::Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            let bytes = self
                .steps
                .pop_front()
                .expect("the relay must not read past the scripted EOF or terminal error")?;
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }
    for limit in [0, 4, 5, 6] {
        let mut reader = ScriptedReader {
            steps: [
                Err(io::Error::from(io::ErrorKind::Interrupted)),
                Ok(b"ab".to_vec()),
                Ok(b"cde".to_vec()),
                Ok(Vec::new()),
            ]
            .into(),
            reads: 0,
        };
        let mut writer = Vec::new();
        assert_eq!(
            capture_stream(&mut reader, &mut writer, limit).unwrap(),
            (limit >= 5).then(|| b"abcde".to_vec())
        );
        assert_eq!(writer, b"abcde");
        assert!(reader.steps.is_empty());
        assert_eq!(reader.reads, 4, "one interrupted read resumes the stream exactly, stopping at EOF");
    }
    let mut empty = ScriptedReader {
        steps: [Ok(Vec::new())].into(),
        reads: 0,
    };
    let mut writer = Vec::new();
    assert_eq!(capture_stream(&mut empty, &mut writer, 0).unwrap(), Some(Vec::new()));
    assert_eq!(writer, b"".as_slice());
    assert_eq!(empty.reads, 1, "an empty stream stops on its first EOF without another read");
    let mut reader = ScriptedReader {
        steps: [Ok(b"ab".to_vec()), Err(io::Error::other("read failed"))].into(),
        reads: 0,
    };
    let mut writer = Vec::new();
    assert_eq!(capture_stream(&mut reader, &mut writer, 5).unwrap_err().to_string(), "read failed");
    assert_eq!(writer, b"ab");
    assert_eq!(reader.reads, 2, "a terminal read error is not retried");
}

pub(super) fn dependency_fixture() -> (MemoryFiles, OwnedUnit, OwnedUnit, Record) {
    let (files, dependency) = fixture("dependency");
    let (consumer_files, mut consumer) = fixture("consumer");
    files.0.borrow_mut().extend(consumer_files.0.into_inner());
    consumer.invocation.externs.push(dependency.target.join("libdependency-hash.rmeta"));
    let record = dependency.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    files.put(dependency.captures.join("dependency.json"), serde_json::to_vec(&record).unwrap());
    (files, consumer, dependency, record)
}

#[test]
fn extern_provenance_requires_current_complete_records_in_the_current_generation() {
    for case in [
        "unchanged",
        "missing-record",
        "malformed-record",
        "other-generation",
        "missing-source-stamp",
        "missing-compiler-stamp",
        "outside-source",
        "changed-source",
        "changed-compiler",
        "changed-output",
        "changed-inventory",
        "changed-search",
        "wrong-output-stamp",
        "relative-extern",
        "outside-extern",
        "parent-extern",
        "bad-metadata",
        "bad-archive",
        "valid-archive",
    ] {
        let (files, mut consumer, dependency, mut record) = dependency_fixture();
        let record_path = dependency.captures.join("dependency.json");
        match case {
            "unchanged" => files.put(dependency.captures.join("ignored.txt"), "not a capture"),
            "missing-record" => {
                let _removed = files.0.borrow_mut().remove(&record_path);
            }
            "malformed-record" => files.put(&record_path, "not JSON"),
            "other-generation" => consumer.captures = dependency.captures.with_file_name("other-generation"),
            "missing-source-stamp" => record.inputs.retain(|(path, _)| path != &record.source),
            "missing-compiler-stamp" => record.inputs.retain(|(path, _)| path != &record.compiler),
            "outside-source" => {
                record.source = drive().join("other").join("lib.rs");
                files.put(&record.source, "fn external() {}");
                record.inputs.push((record.source.clone(), digest(&files, &record.source).unwrap()));
            }
            "changed-source" => files.put(&dependency.invocation.source, "fn changed() {}"),
            "changed-compiler" => files.put(&dependency.compiler, "other compiler"),
            "changed-output" => files.put(dependency.target.join("dependency-hash.d"), "changed dep"),
            "changed-inventory" => files.put(consumer.root.join("new-module.rs"), "fn added() {}"),
            "changed-search" => record.searches.push((dependency.target.clone(), [7; 32])),
            "wrong-output-stamp" => record.outputs[0].1 = [7; 32],
            "relative-extern" => consumer.invocation.externs[0] = "libdependency-hash.rmeta".into(),
            "outside-extern" => consumer.invocation.externs[0] = consumer.root.join("libdependency-hash.rmeta"),
            "parent-extern" => {
                let artifact = consumer.target.join("..").join("libdependency-hash.rmeta");
                files.put(&artifact, b"rust\0metadata");
                record.outputs.push((artifact.clone(), digest(&files, &artifact).unwrap()));
                consumer.invocation.externs[0] = artifact;
            }
            "bad-metadata" => {
                files.put(&consumer.invocation.externs[0], "not metadata");
                for (path, hash) in &mut record.outputs {
                    if path == &consumer.invocation.externs[0] {
                        *hash = digest(&files, path).unwrap();
                    }
                }
            }
            "bad-archive" | "valid-archive" => {
                let artifact = consumer.target.join("libdependency-hash.rlib");
                files.put(
                    &artifact,
                    if case == "valid-archive" {
                        b"!<arch>\narchive".as_slice()
                    } else {
                        b"not an archive".as_slice()
                    },
                );
                record.outputs.push((artifact.clone(), digest(&files, &artifact).unwrap()));
                consumer.invocation.externs = vec![artifact];
            }
            _ => unreachable!(),
        }
        if !matches!(case, "missing-record" | "malformed-record") {
            files.put(&record_path, serde_json::to_vec(&record).unwrap());
        }
        assert_eq!(
            consumer.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_some(),
            matches!(case, "unchanged" | "valid-archive"),
            "{case}"
        );
    }
}

#[test]
fn transitive_graph_closure_hashes_every_artifact_and_terminates_on_shared_cycles() {
    struct BoundedReads<'a> {
        files: &'a MemoryFiles,
        calls: Cell<usize>,
    }
    impl Files for BoundedReads<'_> {
        fn read(&self, path: &Utf8Path) -> Option<Vec<u8>> {
            let calls = self.calls.get();
            assert!(
                calls < 128,
                "the two-artifact cycle must terminate without repeatedly reading its captures"
            );
            self.calls.set(calls + 1);
            self.files.read(path)
        }

        fn files(&self, root: &Utf8Path, excluded: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
            self.files.files(root, excluded)
        }
    }
    let (files, consumer, dependency, mut dependency_record) = dependency_fixture();
    let (leaf_files, leaf) = fixture("leaf");
    files.0.borrow_mut().extend(leaf_files.0.into_inner());
    let leaf_artifact = leaf.target.join("libleaf-hash.rmeta");
    let dependency_artifact = dependency.target.join("libdependency-hash.rmeta");
    dependency_record.inventory = inventory(&files, &consumer.root, &consumer.target).unwrap();
    dependency_record.externs = vec![leaf_artifact.clone(), leaf_artifact.clone()];
    let mut leaf_record = leaf.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    leaf_record.externs.push(dependency_artifact.clone());
    files.put(
        dependency.captures.join("dependency.json"),
        serde_json::to_vec(&dependency_record).unwrap(),
    );
    files.put(leaf.captures.join("leaf.json"), serde_json::to_vec(&leaf_record).unwrap());
    let bounded = BoundedReads {
        files: &files,
        calls: Cell::new(0),
    };
    let snapshot = consumer
        .borrowed()
        .snapshot(&bounded, Vec::new(), Vec::new())
        .expect("a shared cycle is visited only once per artifact");
    for artifact in [&leaf_artifact, &dependency_artifact] {
        assert_eq!(snapshot.inputs.iter().filter(|(path, _)| path == artifact).count(), 1);
    }
    files.put(&leaf.invocation.source, "fn changed() {}");
    assert!(
        !consumer.borrowed().reusable(&files, &snapshot),
        "transitive source provenance must still be current"
    );
    files.put(&leaf.invocation.source, b"pub fn value() -> u32 { 1 }");
    files.put(&leaf_artifact, b"rust\0new leaf");
    assert!(
        consumer.borrowed().snapshot(&files, Vec::new(), Vec::new()).is_none(),
        "an uncaptured transitive artifact cannot close the graph"
    );
}

#[test]
fn source_only_context_filters_stale_records_and_requires_exact_cargo_artifacts() {
    let (files, unit) = fixture("unit");
    let (sibling_files, mut sibling) = fixture("sibling");
    files.0.borrow_mut().extend(sibling_files.0.into_inner());
    sibling.context = [0; 32];
    let record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    let sibling_record = sibling.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
    let artifact = unit.target.join("libunit-hash.rmeta");
    let cargo = serde_json::json!({"reason":"compiler-artifact", "filenames":[artifact]}).to_string();
    let mut stale = record.clone();
    stale.context = [2; 32];
    stale.inventory = [3; 32];
    files.put(unit.captures.join("aaa-stale.json"), serde_json::to_vec(&stale).unwrap());
    files.put(unit.captures.join("unit.json"), serde_json::to_vec(&record).unwrap());
    files.put(unit.captures.join("duplicate.json"), serde_json::to_vec(&record).unwrap());
    files.put(sibling.captures.join("sibling.json"), serde_json::to_vec(&sibling_record).unwrap());
    let context = source_only_context_with(
        &files,
        &unit.captures,
        &format!("{{\"reason\":\"build-finished\"}}\n{cargo}\n{cargo}"),
        &unit.root,
        &unit.target,
        None,
    )
    .unwrap();
    let mut expected: Vec<_> = record
        .inputs
        .iter()
        .filter(|(path, _)| !path.starts_with(&unit.root))
        .cloned()
        .collect();
    expected.sort();
    expected.dedup();
    assert_eq!(context.toolchain, expected);
    assert_eq!(context.units, vec![sibling.context, unit.context]);
    assert!(context.runtime.is_none());
    for stdout in [
        "",
        "not JSON",
        "{}",
        "{\"reason\":3}",
        "{\"reason\":\"compiler-artifact\"}",
        "{\"reason\":\"compiler-artifact\",\"filenames\":3}",
        "{\"reason\":\"compiler-artifact\",\"filenames\":[3]}",
        "{\"reason\":\"compiler-artifact\",\"filenames\":[]}",
        "{\"reason\":\"build-script-executed\"}",
        "{\"reason\":\"compiler-artifact\",\"filenames\":[\"other.rmeta\"]}",
        "{\"reason\":\"compiler-artifact\",\"filenames\":[\"library.rlib\"]}",
    ] {
        assert!(
            source_only_context_with(&files, &unit.captures, stdout, &unit.root, &unit.target, None).is_none(),
            "{stdout}"
        );
    }
    files.put(unit.captures.join("malformed.json"), "{}");
    assert!(source_only_context_with(&files, &unit.captures, &cargo, &unit.root, &unit.target, None).is_none());
}

#[test]
fn record_currency_rejects_missing_stamps_and_toolchain_inventory_changes() {
    for case in [
        "source-stamp",
        "compiler-stamp",
        "source-bytes",
        "output-bytes",
        "search-shape",
        "workspace-shape",
        "new-compiler-file",
        "new-library",
        "wrong-bin",
        "wrong-compiler",
        "unrelated-target-bytes",
    ] {
        let (files, unit) = fixture("unit");
        let mut record = unit.borrowed().snapshot(&files, Vec::new(), Vec::new()).unwrap();
        match case {
            "source-stamp" => record.inputs.retain(|(path, _)| path != &record.source),
            "compiler-stamp" => record.inputs.retain(|(path, _)| path != &record.compiler),
            "source-bytes" => files.put(&unit.invocation.source, "fn changed() {}"),
            "output-bytes" => files.put(unit.target.join("libunit-hash.rmeta"), "new output"),
            "search-shape" => record.searches.push((unit.target.clone(), [7; 32])),
            "workspace-shape" => files.put(unit.root.join("new.rs"), "new source"),
            "new-compiler-file" => files.put(unit.compiler.parent().unwrap().join("rustc-driver.dll"), "new driver"),
            "new-library" => files.put(drive().join("toolchain").join("lib").join("new.rlib"), "new library"),
            "wrong-bin" | "wrong-compiler" => {
                let compiler = if case == "wrong-bin" {
                    drive().join("toolchain").join("other").join("rustc.exe")
                } else {
                    unit.compiler.with_file_name("other.exe")
                };
                files.put(&compiler, "compiler");
                record.compiler = compiler.clone();
                record.inputs.push((compiler.clone(), digest(&files, &compiler).unwrap()));
            }
            _ => {
                let unrelated = unit.target.join("unrelated.rmeta");
                files.put(&unrelated, "before");
                record.inputs.push((unrelated.clone(), digest(&files, &unrelated).unwrap()));
                files.put(&unrelated, "after");
            }
        }

        assert_eq!(
            record_current(&files, &record, &unit.root, &unit.target, &unit.captures),
            if matches!(case, "wrong-bin" | "wrong-compiler") {
                None
            } else {
                Some(case == "unrelated-target-bytes")
            },
            "{case}"
        );
    }
}

#[test]
fn recursive_inventory_is_sorted_excludes_subtrees_and_bounds_all_visited_entries() {
    let root = drive().join("inventory");
    let excluded = root.join("target");
    for (limit, expected) in [
        (3, None),
        (4, None),
        (5, Some(vec![root.join("nested").join("a.rs"), root.join("z.rs")])),
    ] {
        let mut directories = BTreeMap::from([
            (
                root.clone(),
                vec![
                    Some((root.join("z.rs"), "file")),
                    Some((root.join("nested"), "directory")),
                    Some((excluded.clone(), "must-not-inspect")),
                    Some((root.join("target-extra"), "directory")),
                ],
            ),
            (root.join("nested"), vec![Some((root.join("nested").join("a.rs"), "file"))]),
            (root.join("target-extra"), Vec::new()),
        ]);
        let actual = collect_files(
            &root,
            &excluded,
            limit,
            |path| Some(directories.remove(path).expect("every visited directory is scripted once")),
            |entry| entry,
            |kind| {
                Some(match kind {
                    "directory" => SnapshotKind::Directory,
                    "file" => SnapshotKind::File,
                    _ => panic!("excluded entries are never inspected"),
                })
            },
        );
        assert_eq!(actual, expected, "limit {limit} includes excluded entries");
        if limit == 5 {
            assert!(directories.is_empty());
        }
    }
    for kind in ["symlink", "other", "unreadable", "entry-error", "path-error", "directory-error"] {
        let calls = Cell::new(0);
        let result = collect_files(
            &root,
            Utf8Path::new(""),
            10,
            |path| {
                assert_eq!(path, root);
                calls.set(calls.get() + 1);
                if kind == "directory-error" {
                    None
                } else {
                    Some(vec![if kind == "entry-error" {
                        None
                    } else {
                        Some((root.join("entry"), kind))
                    }])
                }
            },
            |entry| {
                if kind == "path-error" { None } else { entry }
            },
            |kind| match kind {
                "symlink" => Some(SnapshotKind::Symlink),
                "other" => Some(SnapshotKind::Other),
                "unreadable" => None,
                _ => panic!("failed paths never reach metadata"),
            },
        );
        assert!(result.is_none(), "{kind}");
        assert_eq!(calls.get(), 1);
    }
    let files = collect_files(
        &root,
        Utf8Path::new(""),
        1,
        |_| Some(vec![(root.join("file"), SnapshotKind::File)]),
        Some,
        Some,
    )
    .unwrap();
    assert_eq!(files, vec![root.join("file")], "empty exclusion does not exclude every path");
}

#[test]
fn path_ambiguity_checks_every_ancestor_and_only_tolerates_missing_entries() {
    let path = drive().join("workspace").join("missing").join("file.rs");
    let ancestors: Vec<_> = path.ancestors().map(Utf8Path::to_owned).collect();
    let mut observed = Vec::new();
    assert!(unambiguous_path_with(&path, |entry| {
        observed.push(entry.to_owned());
        if entry == path {
            Err(io::Error::from(io::ErrorKind::NotFound))
        } else {
            Ok(false)
        }
    }));
    assert_eq!(observed, ancestors);
    for rejected in &ancestors {
        for cause in ["symlink", "permission"] {
            let mut observed = Vec::new();
            assert!(
                !unambiguous_path_with(&path, |entry| {
                    observed.push(entry.to_owned());
                    if entry == rejected {
                        if cause == "symlink" {
                            Ok(true)
                        } else {
                            Err(io::Error::from(io::ErrorKind::PermissionDenied))
                        }
                    } else {
                        Ok(false)
                    }
                }),
                "{rejected}: {cause}"
            );
            assert_eq!(observed.last(), Some(rejected), "inspection stops at the first ambiguous ancestor");
        }
    }
}
