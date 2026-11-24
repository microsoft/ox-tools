// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::VecDeque;
use std::ffi::OsStr;

use super::*;

enum Call {
    Glob(String, bool, Vec<PathBuf>),
    GlobError(String, bool, io::ErrorKind),
    Absolute(PathBuf, io::Result<PathBuf>),
    Read(PathBuf, u64, usize, io::Result<Vec<u8>>),
    File(PathBuf, io::Result<bool>),
    Command(OsString, Vec<String>, io::Result<CommandOutput>),
    Load(PathBuf, Result<(), AppError>),
    Tools(String, Result<(), AppError>),
}

struct Script(VecDeque<Call>);

impl Script {
    fn new(calls: Vec<Call>) -> Self {
        Self(calls.into())
    }
    fn next(&mut self) -> Call {
        self.0.pop_front().expect("every probe operation must have a scripted response")
    }
    fn done(&self) {
        assert!(self.0.is_empty(), "all scripted operations must be consumed");
    }
}

impl Probe for Script {
    fn absolute_path(&mut self, path: &Path) -> io::Result<PathBuf> {
        let Call::Absolute(expected, result) = self.next() else {
            panic!("expected a path-resolution operation");
        };
        assert_eq!(path, expected);
        result
    }

    fn glob(&mut self, pattern: &str, case_sensitive: bool) -> Result<Vec<PathBuf>, AppError> {
        let (expected, expected_case, result) = match self.next() {
            Call::Glob(expected, case, paths) => (expected, case, Ok(paths)),
            Call::GlobError(expected, case, kind) => (expected, case, Err(io::Error::from(kind))),
            _ => panic!("expected a glob operation"),
        };
        assert_eq!(Path::new(pattern), Path::new(&expected));
        assert_eq!(case_sensitive, expected_case);
        result.into_app_err(format!("scripted discovery failure for `{pattern}`"))
    }
    fn is_file(&mut self, path: &Path) -> io::Result<bool> {
        let Call::File(expected, result) = self.next() else {
            panic!("expected an import-library probe");
        };
        assert_eq!(path, expected);
        result
    }
    fn read(&mut self, path: &Path, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        let Call::Read(expected, expected_offset, expected_length, result) = self.next() else {
            panic!("expected a library-header read");
        };
        assert_eq!((path, offset, length), (expected.as_path(), expected_offset, expected_length));
        result
    }
    fn command(&mut self, executable: &OsStr, args: &[&str]) -> io::Result<CommandOutput> {
        let Call::Command(expected, expected_args, result) = self.next() else {
            panic!("expected a discovery command");
        };
        assert_eq!(executable, expected);
        assert_eq!(args, expected_args);
        result
    }
    fn load_library(&mut self, path: &Path) -> Result<(), AppError> {
        let Call::Load(expected, result) = self.next() else {
            panic!("expected a loader check");
        };
        assert_eq!(path, expected);
        result
    }
    fn check_tools(&mut self, target: &Target) -> Result<(), AppError> {
        let Call::Tools(expected, result) = self.next() else {
            panic!("expected a compiler check");
        };
        assert_eq!(target.triple, expected);
        result
    }
}

fn environment(triple: &str, override_path: Option<&str>) -> Environment {
    Environment {
        target: Target::parse(triple).expect("test uses a supported native target"),
        libclang_path: override_path.map(PathBuf::from),
        llvm_config: "llvm-config".into(),
        ld_library_path: Vec::new(),
        path: Vec::new(),
    }
}

fn pattern(directory: &str, name: &str) -> String {
    Path::new(directory).join(name).to_str().expect("test paths are Unicode").to_owned()
}

fn absolute(path: impl Into<PathBuf>) -> Call {
    let path = path.into();
    Call::Absolute(path.clone(), Ok(path))
}

fn linux_search(directory: &str, unversioned: &[&str], versioned: &[&str]) -> Vec<Call> {
    vec![
        Call::Glob(
            pattern(directory, "libclang.so"),
            true,
            unversioned.iter().map(PathBuf::from).collect(),
        ),
        Call::Glob(
            pattern(directory, "libclang-*.so"),
            true,
            versioned.iter().map(PathBuf::from).collect(),
        ),
    ]
}

fn linux_override(files: &[&str]) -> Vec<Call> {
    let mut calls = linux_search("", &[], &[]);
    calls.extend(linux_search("custom", &[], files));
    calls
}

fn elf(path: &str, class: u8) -> Call {
    Call::Read(path.into(), 0, 5, Ok(vec![0x7f, b'E', b'L', b'F', class]))
}

fn pe(path: &Path, machine: u16, magic: u16) -> Vec<Call> {
    let mut header = vec![0; 26];
    header[..4].copy_from_slice(b"PE\0\0");
    header[4..6].copy_from_slice(&machine.to_le_bytes());
    header[24..26].copy_from_slice(&magic.to_le_bytes());
    vec![
        Call::Read(path.to_path_buf(), 0x3c, 4, Ok(128_u32.to_le_bytes().to_vec())),
        Call::Read(path.to_path_buf(), 128, 26, Ok(header)),
    ]
}

#[test]
fn invalid_override_fails_without_searching_other_locations() {
    let environment = environment("x86_64-unknown-linux-gnu", Some("custom"));
    let mut script = Script::new(linux_override(&[]));
    assert!(check_with(&environment, &mut script).is_err());
    script.done();
}

#[test]
fn empty_override_remains_authoritative() {
    let environment = environment("x86_64-unknown-linux-gnu", Some(""));
    let mut script = Script::new(linux_search("", &[], &[]));
    assert!(check_with(&environment, &mut script).is_err());
    script.done();
}

#[test]
fn literal_glob_metacharacters_in_override_directories_are_escaped() {
    let environment = environment("x86_64-unknown-linux-gnu", None);
    let directory = Path::new("LLVM[custom]");
    let path = directory.join("libclang.so");
    let mut script = Script::new(vec![
        Call::Glob(pattern("LLVM[[]custom[]]", "libclang.so"), true, vec![path.clone()]),
        Call::Glob(pattern("LLVM[[]custom[]]", "libclang-*.so"), true, vec![]),
    ]);
    assert_eq!(search_directory(&environment, directory, &mut script).unwrap(), vec![path]);
    script.done();
}

#[test]
fn non_unicode_search_directory_is_rejected_before_any_probe() {
    #[cfg(windows)]
    let directory = {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xd800])
    };
    #[cfg(unix)]
    let directory = {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xff])
    };
    let mut script = Script::new(vec![]);
    search_directory(&environment("x86_64-unknown-linux-gnu", None), Path::new(&directory), &mut script).unwrap_err();
    script.done();
}

#[test]
fn explicit_library_file_does_not_search_other_locations() {
    let path = pattern("custom", "libclang.so");
    let environment = environment("x86_64-unknown-linux-gnu", Some(&path));
    let mut calls = linux_search("custom", &[&path], &[]);
    calls.push(elf(&path, 2));
    let mut script = Script::new(calls);
    assert_eq!(find_libclang(&environment, &mut script).unwrap(), PathBuf::from(path));
    script.done();
}

#[test]
fn version_selection_uses_highest_valid_version_not_first_candidate() {
    let paths = [
        pattern("custom", "libclang-9.so"),
        pattern("custom", "libclang-18.so"),
        pattern("custom", "libclang-20.so"),
    ];
    let mut calls = linux_override(&paths.iter().map(String::as_str).collect::<Vec<_>>());
    calls.extend([elf(&paths[0], 2), elf(&paths[1], 2), elf(&paths[2], 1)]);
    let mut script = Script::new(calls);
    assert_eq!(
        find_libclang(&environment("x86_64-unknown-linux-gnu", Some("custom")), &mut script).unwrap(),
        PathBuf::from(&paths[1])
    );
    script.done();
}

#[test]
fn linux_suffix_sonames_and_cpp_library_are_not_link_time_candidates() {
    let environment = environment("x86_64-unknown-linux-gnu", Some("custom"));
    let cpp = pattern("custom", "libclang-cpp.so");
    let mut script = Script::new(linux_override(&[&cpp]));
    find_libclang(&environment, &mut script).unwrap_err();
    script.done();
}

#[test]
fn loader_and_compiler_failures_are_propagated_in_order() {
    let path = pattern("custom", "libclang-18.so");
    let environment = environment("x86_64-unknown-linux-gnu", Some("custom"));
    for loader_fails in [true, false] {
        let mut calls = linux_override(&[&path]);
        calls.push(elf(&path, 2));
        calls.push(absolute(&path));
        calls.push(Call::Load(
            path.clone().into(),
            if loader_fails {
                Err(io::Error::from(io::ErrorKind::NotFound)).into_app_err("dependent library missing")
            } else {
                Ok(())
            },
        ));
        if !loader_fails {
            calls.push(Call::Tools(
                environment.target.triple.clone(),
                Err(io::Error::from(io::ErrorKind::PermissionDenied)).into_app_err("CXX executable missing"),
            ));
        }
        let mut script = Script::new(calls);
        let error = check_with(&environment, &mut script).unwrap_err();
        assert_eq!(
            error.find_source::<io::Error>().unwrap().kind(),
            if loader_fails {
                io::ErrorKind::NotFound
            } else {
                io::ErrorKind::PermissionDenied
            }
        );
        script.done();
    }
}

#[test]
fn successful_probe_checks_loader_and_native_tools() {
    let path = pattern("custom", "libclang-18.so");
    let environment = environment("x86_64-unknown-linux-gnu", Some("custom"));
    let mut calls = linux_override(&[&path]);
    calls.extend([
        elf(&path, 2),
        absolute(&path),
        Call::Load(path.into(), Ok(())),
        Call::Tools(environment.target.triple.clone(), Ok(())),
    ]);
    let mut script = Script::new(calls);
    check_with(&environment, &mut script).unwrap();
    script.done();
}

#[test]
fn unreadable_optional_search_does_not_discard_valid_library() {
    let environment = environment("x86_64-unknown-linux-gnu", None);
    let path = pattern("llvm/lib", "libclang-18.so");
    let mut calls = vec![Call::Command(
        "llvm-config".into(),
        vec!["--prefix".into()],
        Ok(CommandOutput {
            success: true,
            stdout: "llvm".into(),
            stderr: String::new(),
        }),
    )];
    calls.extend(linux_search("llvm/bin", &[], &[]));
    calls.extend(linux_search("llvm/lib", &[], &[&path]));
    calls.extend(linux_search("llvm/lib64", &[], &[]));
    for (index, glob) in directory_patterns(&environment.target).iter().enumerate() {
        if index == 0 {
            calls.push(Call::Glob((*glob).into(), false, vec!["unreadable".into()]));
            calls.push(Call::GlobError(
                pattern("unreadable", "libclang.so"),
                true,
                io::ErrorKind::PermissionDenied,
            ));
        } else {
            calls.push(Call::Glob((*glob).into(), false, vec![]));
        }
    }
    calls.extend([
        elf(&path, 2),
        absolute(&path),
        Call::Load(path.into(), Ok(())),
        Call::Tools(environment.target.triple.clone(), Ok(())),
    ]);
    let mut script = Script::new(calls);
    check_with(&environment, &mut script).unwrap();
    script.done();
}

#[test]
fn optional_search_failure_retains_first_typed_cause_when_discovery_fails() {
    let environment = environment("x86_64-unknown-linux-gnu", None);
    let mut calls = vec![Call::Command(
        "llvm-config".into(),
        vec!["--prefix".into()],
        Err(io::ErrorKind::NotFound.into()),
    )];
    for (index, glob) in directory_patterns(&environment.target).iter().enumerate() {
        calls.push(match index {
            0 => Call::GlobError((*glob).into(), false, io::ErrorKind::PermissionDenied),
            1 => Call::GlobError((*glob).into(), false, io::ErrorKind::NotFound),
            _ => Call::Glob((*glob).into(), false, vec![]),
        });
    }
    let mut script = Script::new(calls);
    let error = find_libclang(&environment, &mut script).unwrap_err();
    assert_eq!(error.find_source::<io::Error>().unwrap().kind(), io::ErrorKind::PermissionDenied);
    let details = error.to_string();
    for pattern in &directory_patterns(&environment.target)[..2] {
        assert!(details.contains(pattern), "every failed search location must remain diagnosable");
    }
    script.done();
}

#[test]
fn relative_library_filename_is_loaded_as_selected_absolute_file() {
    let environment = environment("x86_64-unknown-linux-gnu", Some("libclang.so"));
    let mut calls = linux_search("", &["libclang.so"], &[]);
    calls.extend([
        elf("libclang.so", 2),
        Call::Absolute("libclang.so".into(), Ok("/repo/libclang.so".into())),
        Call::Load("/repo/libclang.so".into(), Ok(())),
        Call::Tools(environment.target.triple.clone(), Ok(())),
    ]);
    let mut script = Script::new(calls);
    check_with(&environment, &mut script).unwrap();
    script.done();
}

#[test]
fn relative_windows_dll_directory_matches_absolute_path_entry() {
    let mut environment = environment("x86_64-pc-windows-gnu", None);
    environment.path = vec!["C:/repo/llvm/bin".into()];
    let mut script = Script::new(vec![
        Call::Absolute("llvm/bin".into(), Ok("C:/repo/llvm/bin".into())),
        absolute("C:/repo/llvm/bin"),
    ]);
    library::check_link_requirements(&environment, &Path::new("llvm/bin").join("clang.dll"), &mut script).unwrap();
    script.done();
}

#[test]
fn relative_windows_path_entry_matches_absolute_dll_directory() {
    let mut environment = environment("x86_64-pc-windows-gnu", None);
    environment.path = vec!["llvm/bin".into()];
    let mut script = Script::new(vec![
        absolute("C:/repo/llvm/bin"),
        Call::Absolute("llvm/bin".into(), Ok("C:/repo/llvm/bin".into())),
    ]);
    library::check_link_requirements(&environment, &Path::new("C:/repo/llvm/bin").join("clang.dll"), &mut script).unwrap();
    script.done();
}

#[test]
fn selected_library_path_resolution_failure_preserves_cause_without_loading() {
    let path = pattern("custom", "libclang-18.so");
    let environment = environment("x86_64-unknown-linux-gnu", Some("custom"));
    let mut calls = linux_override(&[&path]);
    calls.extend([
        elf(&path, 2),
        Call::Absolute(path.into(), Err(io::ErrorKind::PermissionDenied.into())),
    ]);
    let mut script = Script::new(calls);
    let error = check_with(&environment, &mut script).unwrap_err();
    assert_eq!(error.find_source::<io::Error>().unwrap().kind(), io::ErrorKind::PermissionDenied);
    script.done();
}

#[test]
fn windows_path_resolution_failures_preserve_cause_and_stop_comparison() {
    let mut environment = environment("x86_64-pc-windows-gnu", None);
    environment.path = vec!["llvm/bin".into()];
    for resolve_directory in [false, true] {
        let mut calls = Vec::new();
        if resolve_directory {
            calls.push(absolute("llvm/bin"));
        }
        calls.push(Call::Absolute("llvm/bin".into(), Err(io::ErrorKind::PermissionDenied.into())));
        let mut script = Script::new(calls);
        let error = library::check_link_requirements(&environment, &Path::new("llvm/bin").join("clang.dll"), &mut script).unwrap_err();
        assert_eq!(error.find_source::<io::Error>().unwrap().kind(), io::ErrorKind::PermissionDenied);
        script.done();
    }
}

#[test]
fn llvm_config_and_ld_library_path_are_used_and_earlier_location_wins_ties() {
    let mut environment = environment("x86_64-unknown-linux-gnu", None);
    environment.llvm_config = "custom-llvm-config".into();
    environment.ld_library_path = vec!["loader".into()];
    let llvm = pattern("llvm/lib", "libclang-18.so");
    let loader = pattern("loader", "libclang-18.so");
    let mut calls = vec![Call::Command(
        "custom-llvm-config".into(),
        vec!["--prefix".into()],
        Ok(CommandOutput {
            success: true,
            stdout: "llvm\n".into(),
            stderr: String::new(),
        }),
    )];
    calls.extend(linux_search("llvm/bin", &[], &[]));
    calls.extend(linux_search("llvm/lib", &[], &[&llvm]));
    calls.extend(linux_search("llvm/lib64", &[], &[]));
    calls.extend(linux_search("loader", &[], &[&loader]));
    for pattern in directory_patterns(&environment.target) {
        calls.push(Call::Glob((*pattern).into(), false, vec![]));
    }
    calls.extend([elf(&llvm, 2), elf(&loader, 2)]);
    let mut script = Script::new(calls);
    assert_eq!(find_libclang(&environment, &mut script).unwrap(), PathBuf::from(llvm));
    script.done();
}

#[test]
fn optional_command_failure_allows_platform_glob_discovery() {
    let environment = environment("x86_64-unknown-linux-gnu", None);
    let directory = "/usr/lib/llvm-18/lib";
    let path = pattern(directory, "libclang.so");
    let mut calls = vec![Call::Command(
        "llvm-config".into(),
        vec!["--prefix".into()],
        Err(io::ErrorKind::NotFound.into()),
    )];
    for (index, pattern) in directory_patterns(&environment.target).iter().enumerate() {
        calls.push(Call::Glob(
            (*pattern).into(),
            false,
            if index == 4 { vec![directory.into()] } else { vec![] },
        ));
        if index == 4 {
            calls.extend(linux_search(directory, &[&path], &[]));
        }
    }
    calls.push(elf(&path, 2));
    let mut script = Script::new(calls);
    assert_eq!(find_libclang(&environment, &mut script).unwrap(), PathBuf::from(path));
    script.done();
}

#[test]
fn failed_discovery_command_diagnostic_is_retained_when_no_library_is_found() {
    let environment = environment("x86_64-unknown-linux-gnu", None);
    let mut calls = vec![Call::Command(
        "llvm-config".into(),
        vec!["--prefix".into()],
        Ok(CommandOutput {
            success: false,
            stdout: String::new(),
            stderr: "broken LLVM configuration".into(),
        }),
    )];
    for pattern in directory_patterns(&environment.target) {
        calls.push(Call::Glob((*pattern).into(), false, vec![]));
    }
    let mut script = Script::new(calls);
    let error = find_libclang(&environment, &mut script).unwrap_err();
    assert!(error.to_string().contains("broken LLVM configuration"));
    script.done();
}

#[test]
fn macos_uses_xcode_select_when_llvm_config_is_unavailable() {
    let environment = environment("aarch64-apple-darwin", None);
    let directory = Path::new("Xcode").join("Toolchains/XcodeDefault.xctoolchain/usr/lib");
    let path = directory.join("libclang.dylib");
    let mut calls = vec![
        Call::Command("llvm-config".into(), vec!["--prefix".into()], Err(io::ErrorKind::NotFound.into())),
        Call::Command(
            "xcode-select".into(),
            vec!["--print-path".into()],
            Ok(CommandOutput {
                success: true,
                stdout: "Xcode\n".into(),
                stderr: String::new(),
            }),
        ),
        Call::Glob(directory.join("libclang.dylib").to_str().unwrap().into(), true, vec![path.clone()]),
    ];
    for pattern in directory_patterns(&environment.target) {
        calls.push(Call::Glob((*pattern).into(), false, vec![]));
    }
    let mut script = Script::new(calls);
    assert_eq!(find_libclang(&environment, &mut script).unwrap(), path);
    script.done();
}

#[test]
fn windows_lib_directory_searches_sibling_bin_and_accepts_mingw_import_library() {
    let override_path = Path::new("llvm").join("lib");
    let mut environment = environment("x86_64-pc-windows-msvc", Some(override_path.to_str().unwrap()));
    let bin = Path::new("llvm").join("bin");
    let lib = Path::new("llvm").join("lib");
    let path = bin.join("libclang.dll");
    environment.path = vec![bin];
    let mut calls = vec![
        Call::Glob(pattern("llvm", "clang.dll"), true, vec![]),
        Call::Glob(pattern("llvm", "libclang.dll"), true, vec![]),
        Call::Glob(pattern("llvm/lib", "clang.dll"), true, vec![]),
        Call::Glob(pattern("llvm/lib", "libclang.dll"), true, vec![]),
        Call::Glob(pattern("llvm/bin", "clang.dll"), true, vec![]),
        Call::Glob(pattern("llvm/bin", "libclang.dll"), true, vec![path.clone()]),
    ];
    calls.extend(pe(&path, 0x8664, 0x20b));
    calls.extend([
        Call::File(lib.join("libclang.lib"), Ok(false)),
        Call::File(lib.join("libclang.dll.a"), Ok(true)),
        absolute(path.parent().unwrap()),
        absolute(&environment.path[0]),
        absolute(&path),
        Call::Load(path, Ok(())),
        Call::Tools(environment.target.triple.clone(), Ok(())),
    ]);
    let mut script = Script::new(calls);
    check_with(&environment, &mut script).unwrap();
    script.done();
}

#[test]
fn windows_search_directory_checks_lib_then_sibling_bin() {
    let environment = environment("x86_64-pc-windows-msvc", None);
    let library = Path::new("llvm").join("bin").join("libclang.dll");
    let mut script = Script::new(vec![
        Call::Glob(pattern("llvm/lib", "clang.dll"), true, vec![]),
        Call::Glob(pattern("llvm/lib", "libclang.dll"), true, vec![]),
        Call::Glob(pattern("llvm/bin", "clang.dll"), true, vec![]),
        Call::Glob(pattern("llvm/bin", "libclang.dll"), true, vec![library.clone()]),
    ]);
    assert_eq!(
        search_directory(&environment, Path::new("llvm/lib"), &mut script).unwrap(),
        vec![library]
    );
    script.done();
}

#[test]
fn windows_rejects_missing_import_library_and_missing_path() {
    let library = Path::new("llvm").join("bin").join("libclang.dll");
    let import = Path::new("llvm").join("lib");
    for found_import in [false, true] {
        let mut environment = environment("x86_64-pc-windows-msvc", Some("llvm/bin"));
        if !found_import {
            environment.path = vec![Path::new("llvm").join("bin")];
        }
        let mut calls = vec![
            Call::Glob(pattern("llvm", "clang.dll"), true, vec![]),
            Call::Glob(pattern("llvm", "libclang.dll"), true, vec![]),
            Call::Glob(pattern("llvm/bin", "clang.dll"), true, vec![]),
            Call::Glob(pattern("llvm/bin", "libclang.dll"), true, vec![library.clone()]),
        ];
        calls.extend(pe(&library, 0x8664, 0x20b));
        calls.push(Call::File(import.join("libclang.lib"), Ok(found_import)));
        if found_import {
            calls.push(absolute(library.parent().unwrap()));
        } else {
            calls.push(Call::File(import.join("libclang.dll.a"), Err(io::ErrorKind::NotFound.into())));
        }
        let mut script = Script::new(calls);
        assert!(check_with(&environment, &mut script).is_err());
        script.done();
    }
}

#[test]
fn windows_gnu_needs_no_msvc_import_library_and_normalizes_path() {
    let mut environment = environment("x86_64-pc-windows-gnu", None);
    environment.path = vec!["c:/llvm/bin/".into()];
    let mut script = Script::new(vec![absolute(r"C:\LLVM\bin"), absolute(&environment.path[0])]);
    library::check_link_requirements(&environment, Path::new(r"C:\LLVM\bin").join("clang.dll").as_path(), &mut script).unwrap();
    script.done();
}

#[test]
fn pe_validation_rejects_wrong_architecture_and_pointer_width() {
    let target = Target::parse("aarch64-pc-windows-msvc").unwrap();
    let path = Path::new("libclang.dll");
    for (machine, magic, valid) in [(0x8664, 0x20b, false), (0xaa64, 0x10b, false), (0xaa64, 0x20b, true)] {
        let mut script = Script::new(pe(path, machine, magic));
        if valid {
            library::validate(&target, path, &mut script).unwrap();
        } else {
            assert!(library::validate(&target, path, &mut script).is_err());
        }

        script.done();
    }
}

#[test]
fn malformed_pe_headers_are_rejected_without_import_or_loader_checks() {
    let target = Target::parse("x86_64-pc-windows-msvc").unwrap();
    let path = Path::new("libclang.dll");
    for header in [b"invalid signature".to_vec(), b"PE\0\0".to_vec()] {
        let mut script = Script::new(vec![
            Call::Read(path.into(), 0x3c, 4, Ok(128_u32.to_le_bytes().to_vec())),
            Call::Read(path.into(), 128, 26, Ok(header)),
        ]);
        assert!(library::validate(&target, path, &mut script).is_err());
        script.done();
    }
}

#[test]
fn malformed_or_inaccessible_headers_are_reported_with_candidate_paths() {
    let path = pattern("custom", "libclang-18.so");
    for result in [
        Ok(b"wrong".to_vec()),
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "header permission denied")),
    ] {
        let mut calls = linux_override(&[&path]);
        calls.push(Call::Read(path.clone().into(), 0, 5, result));
        let mut script = Script::new(calls);
        let error = find_libclang(&environment("x86_64-unknown-linux-gnu", Some("custom")), &mut script).unwrap_err();
        assert!(error.to_string().contains(&path));
        script.done();
    }
}

#[test]
fn target_comes_from_caller_rustc_and_cross_installation_is_rejected() {
    let target = installation_target("rustc 1.99.1\nhost: aarch64-pc-windows-msvc\n", None).unwrap();
    assert_eq!((target.arch.as_str(), target.pointer_width, target.msvc), ("aarch64", 64, true));
    installation_target("host: x86_64-unknown-linux-gnu", Some(OsStr::new("aarch64-unknown-linux-gnu"))).unwrap_err();
    installation_target("rustc without host", None).unwrap_err();
}

#[test]
fn target_parsing_accepts_native_widths_and_rejects_unsupported_targets() {
    for (triple, platform, width, msvc) in [
        ("i686-pc-windows-msvc", Platform::Windows, 32, true),
        ("i586-unknown-linux-gnu", Platform::Linux, 32, false),
        ("armv7-unknown-linux-gnueabihf", Platform::Linux, 32, false),
        ("x86_64-unknown-linux-gnux32", Platform::Linux, 32, false),
        ("aarch64-apple-darwin", Platform::MacOs, 64, false),
        ("x86_64-pc-windows-gnu", Platform::Windows, 64, false),
    ] {
        let target = Target::parse(triple).unwrap();
        assert_eq!((target.platform, target.pointer_width, target.msvc), (platform, width, msvc));
        assert_eq!(target.triple, triple);
    }
    for triple in ["x86_64-unknown-freebsd", "wasm32-unknown-unknown", "unknown-unknown-linux-gnu"] {
        Target::parse(triple).unwrap_err();
    }
    let target = installation_target("host: i686-pc-windows-msvc\n", Some(OsStr::new("i686-pc-windows-msvc"))).unwrap();
    assert_eq!(target.pointer_width, 32);
}

#[test]
fn elf_validation_checks_signature_and_both_native_widths() {
    let path = Path::new("libclang.so");
    for (triple, class) in [("x86_64-unknown-linux-gnu", 2), ("i686-unknown-linux-gnu", 1)] {
        let target = Target::parse(triple).unwrap();
        for (header, valid) in [
            (vec![0x7f, b'E', b'L', b'F', class], true),
            (vec![0, b'E', b'L', b'F', class], false),
            (vec![0x7f, b'E', b'L', b'F', 3 - class], false),
            (vec![0x7f, b'E', b'L', b'F'], false),
        ] {
            let mut script = Script::new(vec![Call::Read(path.into(), 0, 5, Ok(header))]);
            assert_eq!(library::validate(&target, path, &mut script).is_ok(), valid);
            script.done();
        }
    }
}

#[test]
fn pe_validation_checks_machine_width_and_accepts_unknown_machine_like_clang_sys() {
    let path = Path::new("libclang.dll");
    for (triple, machine, magic, valid) in [
        ("i686-pc-windows-msvc", 0x14c, 0x10b, true),
        ("i586-pc-windows-msvc", 0x14c, 0x10b, true),
        ("x86_64-pc-windows-msvc", 0x14c, 0x20b, false),
        ("i686-pc-windows-msvc", 0x8664, 0x10b, false),
        ("x86_64-pc-windows-msvc", 0xaa64, 0x20b, false),
        ("i686-pc-windows-msvc", 0x14c, 0x20b, false),
        ("x86_64-pc-windows-msvc", 0xffff, 0x20b, true),
    ] {
        let target = Target::parse(triple).unwrap();
        let mut script = Script::new(pe(path, machine, magic));
        assert_eq!(library::validate(&target, path, &mut script).is_ok(), valid);
        script.done();
    }
}

#[test]
fn pe_header_read_failures_and_truncation_propagate_without_panicking() {
    let target = Target::parse("x86_64-pc-windows-msvc").unwrap();
    let path = Path::new("libclang.dll");
    for (offset, kind) in [
        (Err(io::ErrorKind::PermissionDenied.into()), Some(io::ErrorKind::PermissionDenied)),
        (Ok(vec![128, 0, 0]), None),
    ] {
        let mut script = Script::new(vec![Call::Read(path.into(), 0x3c, 4, offset)]);
        let error = library::validate(&target, path, &mut script).unwrap_err();
        if let Some(kind) = kind {
            assert_eq!(error.find_source::<io::Error>().unwrap().kind(), kind);
        } else {
            assert!(error.find_source::<std::array::TryFromSliceError>().is_some());
        }
        script.done();
    }
    let mut bad_signature = vec![0; 26];
    bad_signature[4..6].copy_from_slice(&0x8664_u16.to_le_bytes());
    bad_signature[24..26].copy_from_slice(&0x20b_u16.to_le_bytes());
    for (header, kind) in [
        (Err(io::ErrorKind::UnexpectedEof.into()), Some(io::ErrorKind::UnexpectedEof)),
        (Ok(b"PE\0\0\x64\x86".to_vec()), None),
        (Ok(bad_signature), None),
    ] {
        let mut script = Script::new(vec![
            Call::Read(path.into(), 0x3c, 4, Ok(128_u32.to_le_bytes().to_vec())),
            Call::Read(path.into(), 128, 26, header),
        ]);
        let error = library::validate(&target, path, &mut script).unwrap_err();
        assert_eq!(error.find_source::<io::Error>().map(io::Error::kind), kind);
        script.done();
    }
}

#[test]
fn version_selection_compares_components_numerically_and_treats_invalid_parts_as_zero() {
    let names = ["libclang-18.9.so", "libclang-18.10.so", "libclang-18.bad.so", "libclang.so"];
    let paths: Vec<_> = names.map(|name| pattern("custom", name)).into_iter().collect();
    let mut calls = linux_override(&paths.iter().map(String::as_str).collect::<Vec<_>>());
    calls.extend(paths.iter().map(|path| elf(path, 2)));
    let mut script = Script::new(calls);
    assert_eq!(
        find_libclang(&environment("x86_64-unknown-linux-gnu", Some("custom")), &mut script).unwrap(),
        PathBuf::from(&paths[1])
    );
    script.done();
    let paths = [pattern("custom", "libclang-18.0.so"), pattern("custom", "libclang-18.bad.so")];
    let mut calls = linux_override(&paths.iter().map(String::as_str).collect::<Vec<_>>());
    calls.extend(paths.iter().map(|path| elf(path, 2)));
    let mut script = Script::new(calls);
    assert_eq!(
        find_libclang(&environment("x86_64-unknown-linux-gnu", Some("custom")), &mut script).unwrap(),
        PathBuf::from(&paths[0])
    );
    script.done();
}

#[test]
fn windows_path_matching_does_not_confuse_different_directories() {
    let mut environment = environment("x86_64-pc-windows-gnu", None);
    let library = Path::new(r"C:\LLVM\bin").join("clang.dll");
    for (entries, valid) in [
        (vec![r"C:\LLVM\bin-extra"], false),
        (vec![r"D:\LLVM\bin"], false),
        (vec![r"C:\LLVM", r"C:\other\bin"], false),
        (vec![r"C:\other", r"c:\LlVm\BiN\\"], true),
        (vec!["c:/LLVM/bin/"], true),
    ] {
        environment.path = entries.into_iter().map(PathBuf::from).collect();
        let mut calls = vec![absolute(library.parent().unwrap())];
        calls.extend(environment.path.iter().map(absolute));
        let mut script = Script::new(calls);
        assert_eq!(library::check_link_requirements(&environment, &library, &mut script).is_ok(), valid);
        script.done();
    }
}

#[test]
fn msvc_import_library_can_be_beside_dll_and_inspection_errors_propagate() {
    let mut environment = environment("x86_64-pc-windows-msvc", None);
    environment.path = vec![PathBuf::from("custom")];
    let path = Path::new("custom").join("libclang.dll");
    let mut script = Script::new(vec![
        Call::File(Path::new("custom").join("libclang.lib"), Ok(true)),
        absolute("custom"),
        absolute("custom"),
    ]);
    library::check_link_requirements(&environment, &path, &mut script).unwrap();
    script.done();
    let mut script = Script::new(vec![
        Call::File(Path::new("custom").join("libclang.lib"), Err(io::ErrorKind::NotFound.into())),
        Call::File(Path::new("custom").join("libclang.dll.a"), Ok(true)),
        absolute("custom"),
        absolute("custom"),
    ]);
    library::check_link_requirements(&environment, &path, &mut script).unwrap();
    script.done();
    let mut script = Script::new(vec![Call::File(
        Path::new("custom").join("libclang.lib"),
        Err(io::ErrorKind::PermissionDenied.into()),
    )]);
    let error = library::check_link_requirements(&environment, &path, &mut script).unwrap_err();
    assert_eq!(error.find_source::<io::Error>().unwrap().kind(), io::ErrorKind::PermissionDenied);
    script.done();
    let mut script = Script::new(vec![]);
    assert!(library::check_link_requirements(&environment, Path::new(""), &mut script).is_err());
    script.done();
}

fn discovery_calls(environment: &Environment) -> Vec<Call> {
    let mut calls = vec![Call::Command(
        environment.llvm_config.clone(),
        vec!["--prefix".into()],
        Ok(CommandOutput {
            success: true,
            stdout: "llvm\n".into(),
            stderr: String::new(),
        }),
    )];
    for directory in ["llvm/bin", "llvm/lib", "llvm/lib64"] {
        calls.extend(linux_search(directory, &[], &[]));
    }
    calls.extend(linux_search("loader", &[], &[]));
    for (index, pattern) in directory_patterns(&environment.target).iter().enumerate() {
        calls.push(Call::Glob(
            (*pattern).into(),
            false,
            if index == 0 { vec!["system".into()] } else { vec![] },
        ));
        if index == 0 {
            calls.extend(linux_search("system", &[], &[]));
        }
    }
    calls
}

fn assert_glob_errors_propagate(environment: &Environment, make_calls: impl Fn() -> Vec<Call>) {
    let count = make_calls().len();
    for index in 0..count {
        let mut calls = make_calls();
        let Call::Glob(pattern, case, _) = calls.remove(index) else {
            continue;
        };
        let directory = Path::new(&pattern).parent().map(Path::to_path_buf);
        calls.insert(index, Call::GlobError(pattern, case, io::ErrorKind::PermissionDenied));
        if environment.libclang_path.is_some() {
            calls.truncate(index + 1);
        } else {
            while matches!(calls.get(index + 1), Some(Call::Glob(next, true, _))
                if !case || Path::new(next).parent() == directory.as_deref())
            {
                calls.remove(index + 1);
            }
        }
        let mut script = Script::new(calls);
        let error = check_with(environment, &mut script).unwrap_err();
        assert_eq!(error.find_source::<io::Error>().unwrap().kind(), io::ErrorKind::PermissionDenied);
        script.done();
    }
}

#[test]
fn discovery_errors_remain_fatal_for_overrides_and_are_retained_for_optional_search() {
    let mut environment = environment("x86_64-unknown-linux-gnu", None);
    environment.ld_library_path = vec!["loader".into()];
    assert_glob_errors_propagate(&environment, || discovery_calls(&environment));
    environment.libclang_path = Some("custom".into());
    assert_glob_errors_propagate(&environment, || linux_override(&[]));
    let environment = self::environment("x86_64-pc-windows-msvc", Some("llvm/lib"));
    assert_glob_errors_propagate(&environment, || {
        ["llvm", "llvm/lib", "llvm/bin"]
            .into_iter()
            .flat_map(|directory| ["clang.dll", "libclang.dll"].map(|name| Call::Glob(pattern(directory, name), true, vec![])))
            .collect()
    });
    let environment = self::environment("aarch64-apple-darwin", None);
    let directory = Path::new("Xcode").join("Toolchains/XcodeDefault.xctoolchain/usr/lib");
    assert_glob_errors_propagate(&environment, || {
        let mut calls = vec![
            Call::Command("llvm-config".into(), vec!["--prefix".into()], Err(io::ErrorKind::NotFound.into())),
            Call::Command(
                "xcode-select".into(),
                vec!["--print-path".into()],
                Ok(CommandOutput {
                    success: true,
                    stdout: "Xcode".into(),
                    stderr: String::new(),
                }),
            ),
            Call::Glob(directory.join("libclang.dylib").to_str().unwrap().into(), true, vec![]),
        ];
        calls.extend(
            directory_patterns(&environment.target)
                .iter()
                .map(|glob| Call::Glob((*glob).into(), false, vec![])),
        );
        calls
    });
}

#[test]
fn optional_discovery_failures_record_one_note_and_never_accept_failed_stdout() {
    for result in [
        Ok(CommandOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        }),
        Ok(CommandOutput {
            success: true,
            stdout: "\nignored".into(),
            stderr: String::new(),
        }),
        Ok(CommandOutput {
            success: false,
            stdout: "unusable-directory".into(),
            stderr: "injected command failure".into(),
        }),
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected execution failure")),
    ] {
        let mut script = Script::new(vec![Call::Command("config".into(), vec!["--prefix".into()], result)]);
        let mut notes = Vec::new();
        assert!(optional_directory_command(&mut script, OsStr::new("config"), &["--prefix"], &mut notes).is_none());
        assert_eq!(notes.len(), 1);
        script.done();
    }
}

#[test]
fn windows_msys_directory_discovery_is_only_enabled_for_gnu() {
    let directory = Path::new("C:/MSYS64/MinGW64/lib");
    let path = directory.join("clang.dll");
    for (triple, should_find) in [("x86_64-pc-windows-gnu", true), ("x86_64-pc-windows-msvc", false)] {
        let environment = environment(triple, None);
        let mut calls = vec![Call::Command(
            "llvm-config".into(),
            vec!["--prefix".into()],
            Err(io::ErrorKind::NotFound.into()),
        )];
        let mut candidates = 0;
        for glob in directory_patterns(&environment.target) {
            // Interpret Windows separators on every host, including Unix test runners.
            let matches = glob::Pattern::new(&glob.replace('\\', "/")).unwrap().matches_path(directory);
            calls.push(Call::Glob(
                (*glob).into(),
                false,
                if matches { vec![directory.into()] } else { vec![] },
            ));
            if matches {
                candidates += 1;
                calls.extend([
                    Call::Glob(directory.join("clang.dll").to_str().unwrap().into(), true, vec![path.clone()]),
                    Call::Glob(directory.join("libclang.dll").to_str().unwrap().into(), true, vec![]),
                    Call::Glob(
                        directory.parent().unwrap().join("bin").join("clang.dll").to_str().unwrap().into(),
                        true,
                        vec![],
                    ),
                    Call::Glob(
                        directory
                            .parent()
                            .unwrap()
                            .join("bin")
                            .join("libclang.dll")
                            .to_str()
                            .unwrap()
                            .into(),
                        true,
                        vec![],
                    ),
                ]);
            }
        }
        for _ in 0..candidates {
            calls.extend(pe(&path, 0x8664, 0x20b));
        }
        let mut script = Script::new(calls);
        let result = find_libclang(&environment, &mut script);
        assert_eq!(result.is_ok(), should_find);
        if let Ok(found) = result {
            assert_eq!(found, path);
        }
        script.done();
    }
}

fn variables(entries: Vec<(&'static str, Option<OsString>)>) -> impl FnMut(&str) -> Option<OsString> {
    let mut entries: VecDeque<_> = entries.into();
    move |name| {
        let (expected, value) = entries.pop_front().expect("every environment lookup must be scripted");
        assert_eq!(name, expected);
        value
    }
}

#[test]
fn bootstrap_rejects_rustc_failure_and_cross_target_before_discovery() {
    for result in [
        Err(io::ErrorKind::NotFound.into()),
        Ok(CommandOutput {
            success: false,
            stdout: "host: x86_64-unknown-linux-gnu".into(),
            stderr: "rustc failure".into(),
        }),
    ] {
        let mut script = Script::new(vec![Call::Command("rustc".into(), vec!["-vV".into()], result)]);
        assert!(check_environment(&mut script, |_| panic!("failed rustc must not read configuration")).is_err());
        script.done();
    }
    let mut script = Script::new(vec![Call::Command(
        "rustc".into(),
        vec!["-vV".into()],
        Ok(CommandOutput {
            success: true,
            stdout: "host: x86_64-unknown-linux-gnu".into(),
            stderr: String::new(),
        }),
    )]);
    assert!(
        check_environment(
            &mut script,
            variables(vec![("CARGO_BUILD_TARGET", Some("aarch64-unknown-linux-gnu".into()))])
        )
        .is_err()
    );
    script.done();
}

#[test]
fn bootstrap_uses_owned_override_and_path_values_without_host_environment() {
    let path = Path::new("custom").join("libclang.dll");
    let mut calls = vec![Call::Command(
        "rustc".into(),
        vec!["-vV".into()],
        Ok(CommandOutput {
            success: true,
            stdout: "host: x86_64-pc-windows-msvc".into(),
            stderr: String::new(),
        }),
    )];
    calls.extend([
        Call::Glob(pattern("", "clang.dll"), true, vec![]),
        Call::Glob(pattern("", "libclang.dll"), true, vec![]),
        Call::Glob(pattern("custom", "clang.dll"), true, vec![]),
        Call::Glob(pattern("custom", "libclang.dll"), true, vec![path.clone()]),
    ]);
    calls.extend(pe(&path, 0x8664, 0x20b));
    calls.extend([
        Call::File(Path::new("custom").join("libclang.lib"), Ok(true)),
        absolute("custom"),
        absolute("other"),
        absolute("custom"),
        absolute(&path),
        Call::Load(path, Ok(())),
        Call::Tools("x86_64-pc-windows-msvc".into(), Ok(())),
    ]);
    let mut script = Script::new(calls);
    let path_value = std::env::join_paths([Path::new("other"), Path::new("custom")]).unwrap();
    check_environment(
        &mut script,
        variables(vec![
            ("CARGO_BUILD_TARGET", None),
            ("LIBCLANG_PATH", Some("custom".into())),
            ("LLVM_CONFIG_PATH", None),
            ("LD_LIBRARY_PATH", None),
            ("PATH", Some(path_value)),
        ]),
    )
    .unwrap();
    script.done();
}

#[test]
fn bootstrap_accepts_native_cargo_host_alias() {
    let environment = environment("x86_64-unknown-linux-gnu", Some("custom"));
    let path = pattern("custom", "libclang-18.so");
    let mut calls = ["rustc", "cargo"]
        .into_iter()
        .map(|tool| {
            Call::Command(
                tool.into(),
                vec!["-vV".into()],
                Ok(CommandOutput {
                    success: true,
                    stdout: "host: x86_64-unknown-linux-gnu".into(),
                    stderr: String::new(),
                }),
            )
        })
        .collect::<Vec<_>>();
    calls.extend(linux_override(&[&path]));
    calls.extend([
        elf(&path, 2),
        absolute(&path),
        Call::Load(path.into(), Ok(())),
        Call::Tools(environment.target.triple, Ok(())),
    ]);
    let mut script = Script::new(calls);
    check_environment(
        &mut script,
        variables(vec![
            ("CARGO_BUILD_TARGET", Some("host-tuple".into())),
            ("LIBCLANG_PATH", Some("custom".into())),
            ("LLVM_CONFIG_PATH", None),
            ("LD_LIBRARY_PATH", None),
            ("PATH", None),
        ]),
    )
    .unwrap();
    script.done();
}

#[test]
fn bootstrap_rejects_cargo_host_alias_if_cargo_and_rustc_hosts_differ() {
    let mut script = Script::new(
        [("rustc", "x86_64-unknown-linux-gnu"), ("cargo", "aarch64-unknown-linux-gnu")]
            .map(|(tool, host)| {
                Call::Command(
                    tool.into(),
                    vec!["-vV".into()],
                    Ok(CommandOutput {
                        success: true,
                        stdout: format!("host: {host}"),
                        stderr: String::new(),
                    }),
                )
            })
            .into(),
    );
    check_environment(&mut script, variables(vec![("CARGO_BUILD_TARGET", Some("host-tuple".into()))])).unwrap_err();
    script.done();
}

#[test]
fn bootstrap_cargo_alias_failure_preserves_typed_cause() {
    let mut script = Script::new(vec![
        Call::Command(
            "rustc".into(),
            vec!["-vV".into()],
            Ok(CommandOutput {
                success: true,
                stdout: "host: x86_64-unknown-linux-gnu".into(),
                stderr: String::new(),
            }),
        ),
        Call::Command("cargo".into(), vec!["-vV".into()], Err(io::ErrorKind::NotFound.into())),
    ]);
    let error = check_environment(&mut script, variables(vec![("CARGO_BUILD_TARGET", Some("host-tuple".into()))])).unwrap_err();
    assert_eq!(error.find_source::<io::Error>().unwrap().kind(), io::ErrorKind::NotFound);
    script.done();
}

#[test]
fn bootstrap_rejects_failed_or_missing_cargo_host_output_before_discovery() {
    for (success, stdout) in [(false, "host: x86_64-unknown-linux-gnu"), (true, "cargo version only")] {
        let mut script = Script::new(vec![
            Call::Command(
                "rustc".into(),
                vec!["-vV".into()],
                Ok(CommandOutput {
                    success: true,
                    stdout: "host: x86_64-unknown-linux-gnu".into(),
                    stderr: String::new(),
                }),
            ),
            Call::Command(
                "cargo".into(),
                vec!["-vV".into()],
                Ok(CommandOutput {
                    success,
                    stdout: stdout.into(),
                    stderr: "injected cargo failure".into(),
                }),
            ),
        ]);
        check_environment(&mut script, variables(vec![("CARGO_BUILD_TARGET", Some("host-tuple".into()))])).unwrap_err();
        script.done();
    }
}

#[test]
fn bootstrap_passes_llvm_override_and_loader_paths_to_discovery() {
    for llvm_override in [None, Some("selected-config")] {
        let mut environment = environment("x86_64-unknown-linux-gnu", None);
        environment.llvm_config = llvm_override.unwrap_or("llvm-config").into();
        environment.ld_library_path = vec!["loader".into()];
        let mut calls = vec![Call::Command(
            "rustc".into(),
            vec!["-vV".into()],
            Ok(CommandOutput {
                success: true,
                stdout: "host: x86_64-unknown-linux-gnu".into(),
                stderr: String::new(),
            }),
        )];
        calls.extend(discovery_calls(&environment));
        let mut script = Script::new(calls);
        assert!(
            check_environment(
                &mut script,
                variables(vec![
                    ("CARGO_BUILD_TARGET", None),
                    ("LIBCLANG_PATH", None),
                    ("LLVM_CONFIG_PATH", llvm_override.map(OsString::from)),
                    ("LD_LIBRARY_PATH", Some(std::env::join_paths([Path::new("loader")]).unwrap())),
                    ("PATH", None),
                ])
            )
            .is_err()
        );
        script.done();
    }
}
