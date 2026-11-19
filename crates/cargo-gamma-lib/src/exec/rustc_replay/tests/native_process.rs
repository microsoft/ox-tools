// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::process_io::{self, Output, Script};
use super::*;

fn compiler_command(unit: &OwnedUnit) -> Command {
    let mut command = Command::new(&unit.compiler);
    let _command = command.args([
        OsString::from("--crate-name=unit"),
        OsString::from("--crate-type=lib"),
        OsString::from("--emit=dep-info,metadata"),
        OsString::from("-Cextra-filename=-hash"),
        OsString::from("--out-dir"),
        unit.target.clone().into(),
        unit.invocation.source.clone().into(),
    ]);
    command
}

fn script(files: &MemoryFiles, unit: &OwnedUnit) -> snapshot_io::Script {
    let mut script = snapshot_io::Script::from_files(
        files
            .0
            .borrow()
            .iter()
            .map(|(path, bytes)| (path.clone().into_std_path_buf(), bytes.clone())),
    );
    script.cwd = Some(unit.cwd.clone().into_std_path_buf());
    script.environment = [
        (REPLAY_VAR, "a".repeat(64)),
        (ROOT_VAR, unit.root.to_string()),
        (TARGET_VAR, unit.target.to_string()),
        (CAPTURE_DIR_VAR, unit.captures.parent().unwrap().parent().unwrap().to_string()),
    ]
    .into_iter()
    .map(|(name, value)| (name.into(), value.into()))
    .collect();
    script
}

fn output(code: i32) -> Output {
    Output {
        code: Ok(Some(code)),
        stdout: b"metadata output\n".to_vec(),
        stderr: b"compiler diagnostic\n".to_vec(),
        read_failure: None,
    }
}

fn invalid_os() -> OsString {
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

#[test]
fn native_runtime_provenance_is_published_only_after_a_successful_compiler_status() {
    for replay in [false, true] {
        for result in [Ok(Some(0)), Ok(Some(1)), Err(io::ErrorKind::PermissionDenied)] {
            let (files, unit, command, environment) = super::runtime_regressions::runtime_fixture();
            let mut filesystem = script(&files, &unit);
            filesystem
                .environment
                .extend(environment.into_iter().map(|(key, value)| (key.into(), value.into())));
            let mut command = command;
            if replay {
                let mut args = command.get_args().map(OsString::from).collect::<Vec<_>>();
                *args.iter_mut().find(|arg| *arg == "--emit=dep-info,metadata,link").unwrap() = "--emit=dep-info,metadata".into();
                command = Command::new(&unit.compiler);
                let _command = command.args(args);
            }
            let processes = if replay {
                Script {
                    children: [result.map(|code| output(code.unwrap()))].into(),
                    ..Script::default()
                }
            } else {
                Script {
                    statuses: [result].into(),
                    ..Script::default()
                }
            };
            let directory = unit.captures.parent().unwrap().parent().unwrap();
            snapshot_io::with(filesystem, || {
                process_io::with(processes, |_| {
                    assert!(runtime::load(&NativeFiles, directory).is_none());
                    assert_eq!(compiler_status(&mut command).map_err(|error| error.kind()), result);
                    assert_eq!(
                        runtime::load(&NativeFiles, directory).is_some(),
                        result == Ok(Some(0)),
                        "replay={replay}, result={result:?}"
                    );
                });
            });
        }
    }
}

#[test]
fn native_preparation_and_non_unicode_environment_decline_without_panicking() {
    for boundary in ["name", "value", "cwd", "cwd-unicode", "program"] {
        let (files, unit) = fixture("unit");
        let mut filesystem = script(&files, &unit);
        let mut command = compiler_command(&unit);
        match boundary {
            "name" => {
                let _previous = filesystem.environment.insert(invalid_os(), "value".into());
            }
            "value" => {
                let _previous = filesystem.environment.insert("name".into(), invalid_os());
            }
            "cwd" => filesystem.cwd = None,
            "cwd-unicode" => filesystem.cwd = Some(std::path::PathBuf::from(invalid_os())),
            _ => {
                let args = command.get_args().map(OsString::from).collect::<Vec<_>>();
                command = Command::new(invalid_os());
                let _command = command.args(args);
            }
        }
        let processes = Script {
            statuses: [Ok(Some(9))].into(),
            ..Script::default()
        };
        snapshot_io::with(filesystem, || {
            process_io::with(processes, |state| {
                assert_eq!(compiler_status(&mut command).unwrap(), Some(9), "{boundary}");
                assert_eq!(state.lock().unwrap().calls, ["status"], "{boundary}");
            });
        });
    }
}

#[test]
fn preparation_requires_distinct_absolute_roots_and_accepts_nested_working_and_output_directories() {
    let (_files, mut unit) = fixture("unit");
    unit.cwd = unit.root.join("nested");
    unit.target = unit.target.join("nested");
    let command = compiler_command(&unit);
    let environment = BTreeMap::from([
        (REPLAY_VAR.to_owned(), "a".repeat(64)),
        (ROOT_VAR.to_owned(), unit.root.to_string()),
        (TARGET_VAR.to_owned(), unit.target.parent().unwrap().to_string()),
        (CAPTURE_DIR_VAR.to_owned(), unit.captures.to_string()),
    ]);
    assert!(prepare_with(&command, &environment, &unit.cwd).is_some());
    for boundary in ["root", "target", "equal"] {
        let mut environment = environment.clone();
        let (key, value) = match boundary {
            "root" => (ROOT_VAR, String::new()),
            "target" => (TARGET_VAR, String::new()),
            _ => (TARGET_VAR, unit.root.to_string()),
        };
        let _previous = environment.insert(key.into(), value);
        let mut command = compiler_command(&unit);
        if boundary == "equal" {
            let mut args = command.get_args().map(OsString::from).collect::<Vec<_>>();
            let index = args.iter().position(|arg| arg == "--out-dir").unwrap() + 1;
            args[index] = unit.root.join("outputs").into();
            command = Command::new(&unit.compiler);
            let _command = command.args(args);
        }
        assert!(prepare_with(&command, &environment, &unit.cwd).is_none(), "{boundary}");
    }
}

#[test]
fn native_compiler_status_reuses_only_a_published_success_and_relays_both_streams() {
    let (files, unit) = fixture("unit");
    snapshot_io::with(script(&files, &unit), || {
        let mut command = compiler_command(&unit);
        let processes = Script {
            children: [Ok(output(0))].into(),
            ..Script::default()
        };
        process_io::with(processes, |state| {
            assert_eq!(compiler_status(&mut command).unwrap(), Some(0));
            let (directory, _) = prepare(
                &command,
                &capture_env::vars_os()
                    .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
                    .collect::<BTreeMap<_, _>>(),
            )
            .unwrap();
            assert_eq!(NativeFiles.files(&directory, Utf8Path::new("")).unwrap().len(), 1);
            assert_eq!(compiler_status(&mut command).unwrap(), Some(0));
            let state = state.lock().unwrap();
            assert_eq!(state.calls, ["spawn", "wait"]);
            assert_eq!(state.stdout, b"metadata output\nmetadata output\n");
            assert_eq!(state.stderr, b"compiler diagnostic\ncompiler diagnostic\n");
            assert_eq!(state.children.len(), 0);
        });
    });
}

#[test]
fn native_compiler_status_preserves_failures_and_does_not_publish_them() {
    let (files, unit) = fixture("unit");
    snapshot_io::with(script(&files, &unit), || {
        let mut command = compiler_command(&unit);
        let processes = Script {
            children: [Ok(output(1)), Ok(output(0))].into(),
            ..Script::default()
        };
        process_io::with(processes, |state| {
            assert_eq!(compiler_status(&mut command).unwrap(), Some(1));
            assert_eq!(compiler_status(&mut command).unwrap(), Some(0));
            assert_eq!(state.lock().unwrap().calls, ["spawn", "wait", "spawn", "wait"]);
        });
    });
}

#[test]
fn native_compiler_status_preserves_spawn_wait_read_and_relay_errors() {
    for boundary in ["spawn", "wait", "read", "relay", "cached-relay"] {
        let (files, unit) = fixture("unit");
        snapshot_io::with(script(&files, &unit), || {
            let mut child = output(0);
            match boundary {
                "wait" => child.code = Err(io::ErrorKind::PermissionDenied),
                "read" => child.read_failure = Some(io::ErrorKind::PermissionDenied),
                _ => {}
            }
            let processes = Script {
                children: [if boundary == "spawn" {
                    Err(io::ErrorKind::PermissionDenied)
                } else {
                    Ok(child)
                }]
                .into(),
                write_failure: (boundary == "relay").then_some(io::ErrorKind::PermissionDenied),
                ..Script::default()
            };
            process_io::with(processes, |state| {
                let mut command = compiler_command(&unit);
                if boundary == "cached-relay" {
                    assert_eq!(compiler_status(&mut command).unwrap(), Some(0));
                    state.lock().unwrap().write_failure = Some(io::ErrorKind::PermissionDenied);
                }
                assert_eq!(
                    compiler_status(&mut command).unwrap_err().kind(),
                    io::ErrorKind::PermissionDenied,
                    "{boundary}"
                );
                let state = state.lock().unwrap();
                if matches!(boundary, "read" | "relay") {
                    assert!(state.calls.contains(&"kill"), "{boundary}");
                }
                assert_eq!(state.children.len(), 0);
            });
        });
    }
}

#[test]
fn native_stream_capture_marks_either_oversized_stream_uncapturable_without_losing_output() {
    for (stdout, stderr) in [
        (MAX_STREAM_BYTES, 3),
        (3, MAX_STREAM_BYTES),
        (MAX_STREAM_BYTES + 1, 3),
        (3, MAX_STREAM_BYTES + 1),
    ] {
        let processes = Script {
            children: [Ok(Output {
                stdout: vec![b'o'; stdout],
                stderr: vec![b'e'; stderr],
                ..output(0)
            })]
            .into(),
            ..Script::default()
        };
        process_io::with(processes, |state| {
            let captured = capture_output(&mut Command::new("scripted-rustc")).unwrap();
            assert_eq!(captured.code, Some(0));
            assert_eq!(captured.capturable, stdout <= MAX_STREAM_BYTES && stderr <= MAX_STREAM_BYTES);
            assert!(captured.forwarded);
            assert_eq!(captured.stdout.len(), if stdout <= MAX_STREAM_BYTES { stdout } else { 0 });
            assert_eq!(captured.stderr.len(), if stderr <= MAX_STREAM_BYTES { stderr } else { 0 });
            let state = state.lock().unwrap();
            assert_eq!(state.stdout, vec![b'o'; stdout]);
            assert_eq!(state.stderr, vec![b'e'; stderr]);
        });
    }
}

#[test]
fn native_compiler_status_falls_back_without_replay_and_preserves_status_errors() {
    for result in [Ok(Some(0)), Ok(Some(7)), Err(io::ErrorKind::PermissionDenied)] {
        let processes = Script {
            statuses: [result].into(),
            ..Script::default()
        };
        snapshot_io::with(snapshot_io::Script::default(), || {
            process_io::with(processes, |state| {
                let actual = compiler_status(&mut Command::new("scripted-rustc")).map_err(|error| error.kind());
                assert_eq!(actual, result);
                assert_eq!(state.lock().unwrap().calls, ["status"]);
            });
        });
    }
}
