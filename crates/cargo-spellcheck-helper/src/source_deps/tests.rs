// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::VecDeque;
use std::ffi::OsStr;

use super::*;

enum Call {
    Glob(String, bool, Vec<PathBuf>),
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
    fn glob(&mut self, pattern: &str, case_sensitive: bool) -> Result<Vec<PathBuf>, AppError> {
        let Call::Glob(expected, expected_case, paths) = self.next() else {
            panic!("expected a glob operation");
        };
        assert_eq!(Path::new(pattern), Path::new(&expected));
        assert_eq!(case_sensitive, expected_case);
        Ok(paths)
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
fn invalid_override_is_authoritative_and_reports_how_to_fix_it() {
    let environment = environment("x86_64-unknown-linux-gnu", Some("custom"));
    let mut script = Script::new(linux_override(&[]));
    let error = find_libclang(&environment, &mut script).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("LIBCLANG_PATH `custom` is authoritative; fix or unset it")
    );
    script.done();
}

#[test]
fn empty_override_remains_authoritative() {
    let environment = environment("x86_64-unknown-linux-gnu", Some(""));
    let mut script = Script::new(linux_search("", &[], &[]));
    let error = find_libclang(&environment, &mut script).unwrap_err();
    assert!(error.to_string().contains("LIBCLANG_PATH `` is authoritative"));
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
    let error = find_libclang(&environment, &mut script).unwrap_err();
    assert!(error.to_string().contains("No valid link-time libclang"));
    script.done();
}

#[test]
fn loader_and_compiler_failures_are_propagated_in_order() {
    let path = pattern("custom", "libclang-18.so");
    let environment = environment("x86_64-unknown-linux-gnu", Some("custom"));
    for loader_fails in [true, false] {
        let mut calls = linux_override(&[&path]);
        calls.push(elf(&path, 2));
        calls.push(Call::Load(
            path.clone().into(),
            if loader_fails {
                Err(AppError::new("dependent library missing"))
            } else {
                Ok(())
            },
        ));
        if !loader_fails {
            calls.push(Call::Tools(
                environment.target.triple.clone(),
                Err(AppError::new("CXX executable missing")),
            ));
        }
        let mut script = Script::new(calls);
        let error = check_with(&environment, &mut script).unwrap_err();
        assert!(error.to_string().contains(if loader_fails {
            "dependent library missing"
        } else {
            "CXX executable missing"
        }));
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
        Call::Load(path.into(), Ok(())),
        Call::Tools(environment.target.triple.clone(), Ok(())),
    ]);
    let mut script = Script::new(calls);
    check_with(&environment, &mut script).unwrap();
    script.done();
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
    assert!(error.to_string().contains("llvm-config --prefix failed: broken LLVM configuration"));
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
    let environment = environment("x86_64-pc-windows-msvc", None);
    let library = Path::new("llvm").join("bin").join("libclang.dll");
    let import = Path::new("llvm").join("lib");
    for found_import in [false, true] {
        let mut calls = vec![Call::File(import.join("libclang.lib"), Ok(found_import))];
        if !found_import {
            calls.push(Call::File(import.join("libclang.dll.a"), Err(io::ErrorKind::NotFound.into())));
        }
        let mut script = Script::new(calls);
        let error = library::check_link_requirements(&environment, &library, &mut script).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(if found_import { "not on PATH" } else { "for MSVC linking" })
        );
        script.done();
    }
}

#[test]
fn windows_gnu_needs_no_msvc_import_library_and_normalizes_path() {
    let mut environment = environment("x86_64-pc-windows-gnu", None);
    environment.path = vec!["c:/llvm/bin/".into()];
    let mut script = Script::new(vec![]);
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
            let error = library::validate(&target, path, &mut script).unwrap_err();
            assert!(error.to_string().contains("DLL architecture does not match"));
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
        let error = library::validate(&target, path, &mut script).unwrap_err();
        assert!(error.to_string().contains("PE"));
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
        assert!(error.to_string().contains("ELF"));
        script.done();
    }
}

#[test]
fn target_comes_from_caller_rustc_and_cross_installation_is_rejected() {
    let target = installation_target("rustc 1.99.1\nhost: aarch64-pc-windows-msvc\n", None).unwrap();
    assert_eq!((target.arch.as_str(), target.pointer_width, target.msvc), ("aarch64", 64, true));
    let error = installation_target("host: x86_64-unknown-linux-gnu", Some(OsStr::new("aarch64-unknown-linux-gnu"))).unwrap_err();
    assert!(error.to_string().contains("native cargo-spellcheck installations only"));
    let error = installation_target("rustc without host", None).unwrap_err();
    assert!(error.to_string().contains("did not report a host target"));
}
