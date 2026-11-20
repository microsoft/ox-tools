// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::{fs, io};

use ohno::{AppError, IntoAppError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Windows,
    Linux,
    MacOs,
}

#[derive(Debug)]
struct Environment {
    platform: Platform,
    libclang_path: Option<PathBuf>,
    user_profile: Option<PathBuf>,
    path: Vec<PathBuf>,
}

#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn check() -> Result<(), AppError> {
    let platform = if cfg!(windows) {
        Platform::Windows
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        return Err(AppError::new(
            "source prerequisite checks support Windows, Linux, and macOS; this platform is unsupported",
        ));
    };
    let environment = Environment {
        platform,
        libclang_path: std::env::var_os("LIBCLANG_PATH")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        user_profile: std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()).map(PathBuf::from),
        path: std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default(),
    };
    check_with(&environment, is_file, directory_entries)
}

fn check_with(
    environment: &Environment,
    mut is_file: impl FnMut(&Path) -> io::Result<bool>,
    mut directory_entries: impl FnMut(&Path) -> io::Result<Vec<OsString>>,
) -> Result<(), AppError> {
    let directory = find_libclang(environment, &mut is_file, &mut directory_entries)?
        .ok_or_else(|| AppError::new(missing_libclang_message(environment.platform)))?;

    if environment.platform == Platform::Windows && !environment.path.iter().any(|entry| windows_path_matches(entry, &directory)) {
        return Err(AppError::new(format!(
            "libclang.dll exists in `{}` but that directory is not on PATH.\n\
             cargo-spellcheck source builds need libclang on PATH as well as LIBCLANG_PATH.\n\
             Set LIBCLANG_PATH to `{}` and add that directory to PATH before retrying setup.",
            directory.display(),
            directory.display(),
        )));
    }
    Ok(())
}

fn find_libclang(
    environment: &Environment,
    is_file: &mut impl FnMut(&Path) -> io::Result<bool>,
    directory_entries: &mut impl FnMut(&Path) -> io::Result<Vec<OsString>>,
) -> Result<Option<PathBuf>, AppError> {
    let library_names: &[&str] = match environment.platform {
        Platform::Windows => &["libclang.dll"],
        Platform::Linux => &["libclang.so", "libclang.so.1"],
        Platform::MacOs => &["libclang.dylib"],
    };
    let mut directories = Vec::new();
    if let Some(directory) = &environment.libclang_path {
        directories.push(directory.clone());
    }
    match environment.platform {
        Platform::Windows => {
            directories.push(PathBuf::from(r"C:\Program Files\LLVM\bin"));
            if let Some(profile) = &environment.user_profile {
                directories.push(profile.join(r"scoop\apps\llvm\current\bin"));
            }
        }
        Platform::Linux => directories.extend(["/usr/lib/x86_64-linux-gnu", "/usr/lib/aarch64-linux-gnu", "/usr/lib64"].map(PathBuf::from)),
        Platform::MacOs => directories.extend(["/usr/local/opt/llvm/lib", "/opt/homebrew/opt/llvm/lib"].map(PathBuf::from)),
    }
    for directory in directories {
        for name in library_names {
            if probe_file(&directory.join(name), is_file)? {
                return Ok(Some(directory));
            }
        }
    }

    if environment.platform == Platform::Linux {
        for directory in ["/usr/lib", "/usr/lib64", "/usr/lib/x86_64-linux-gnu", "/usr/lib/aarch64-linux-gnu"] {
            let directory = Path::new(directory);
            let mut entries = match directory_entries(directory) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).into_app_err(format!("failed to search libclang directory `{}`", directory.display())),
            };
            entries.sort_unstable();
            for entry in entries {
                if entry.to_str().is_some_and(is_linux_library_name) && probe_file(&directory.join(entry), is_file)? {
                    return Ok(Some(directory.to_path_buf()));
                }
            }
        }
    }
    Ok(None)
}

fn probe_file(path: &Path, is_file: &mut impl FnMut(&Path) -> io::Result<bool>) -> Result<bool, AppError> {
    match is_file(path) {
        Ok(found) => Ok(found),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).into_app_err(format!("failed to inspect libclang candidate `{}`", path.display())),
    }
}

fn is_linux_library_name(name: &str) -> bool {
    name.starts_with("libclang") && name.contains(".so")
}

fn windows_path_matches(left: &Path, right: &Path) -> bool {
    let normalize = |path: &Path| {
        let mut bytes: Vec<_> = path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .map(|byte| if *byte == b'/' { b'\\' } else { byte.to_ascii_lowercase() })
            .collect();
        while bytes.last() == Some(&b'\\') {
            bytes.pop();
        }
        bytes
    };
    normalize(left) == normalize(right)
}

fn missing_libclang_message(platform: Platform) -> &'static str {
    match platform {
        Platform::Windows => {
            "missing source-build dependency: libclang.dll.\n\
             cargo-spellcheck (clang-sys/hunspell-rs) needs libclang at build time.\n\
             Install LLVM with `scoop install llvm`, or `winget install LLVM.LLVM` (may require administrator rights).\n\
             Set LIBCLANG_PATH to the LLVM bin directory and add that directory to PATH, then retry setup."
        }
        Platform::Linux => {
            "missing source-build dependency: libclang.\n\
             cargo-spellcheck (clang-sys/hunspell-rs) needs libclang at build time.\n\
             Ubuntu/Debian: `sudo apt-get install -y libclang-dev`.\n\
             Azure Linux: `sudo tdnf install -y clang-devel`; RHEL: `sudo dnf install -y clang-devel`.\n\
             For a nonstandard installation, set LIBCLANG_PATH to its library directory, then retry setup."
        }
        Platform::MacOs => {
            "missing source-build dependency: libclang.dylib.\n\
             cargo-spellcheck (clang-sys/hunspell-rs) needs libclang at build time.\n\
             Install LLVM with `brew install llvm`.\n\
             For a nonstandard installation, set LIBCLANG_PATH to its library directory, then retry setup."
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn is_file(path: &Path) -> io::Result<bool> {
    fs::metadata(path).map(|metadata| metadata.is_file())
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn directory_entries(path: &Path) -> io::Result<Vec<OsString>> {
    fs::read_dir(path)?.map(|entry| entry.map(|entry| entry.file_name())).collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    type FileProbe = (PathBuf, io::Result<bool>);
    type DirectoryProbe = (PathBuf, io::Result<Vec<OsString>>);

    fn environment(platform: Platform) -> Environment {
        Environment {
            platform,
            libclang_path: None,
            user_profile: None,
            path: Vec::new(),
        }
    }

    fn check_scripted(environment: &Environment, files: Vec<FileProbe>, directories: Vec<DirectoryProbe>) -> Result<(), AppError> {
        let mut files: VecDeque<_> = files.into();
        let mut directories: VecDeque<_> = directories.into();
        let result = check_with(
            environment,
            |path| {
                let (expected, result) = files.pop_front().expect("every file probe must be scripted");
                assert_eq!(path, expected);
                result
            },
            |path| {
                let (expected, result) = directories.pop_front().expect("every directory probe must be scripted");
                assert_eq!(path, expected);
                result
            },
        );
        assert!(files.is_empty(), "all scripted file probes must be used");
        assert!(directories.is_empty(), "all scripted directory probes must be used");
        result
    }

    fn absent_linux_files() -> Vec<FileProbe> {
        [
            "/usr/lib/x86_64-linux-gnu/libclang.so",
            "/usr/lib/x86_64-linux-gnu/libclang.so.1",
            "/usr/lib/aarch64-linux-gnu/libclang.so",
            "/usr/lib/aarch64-linux-gnu/libclang.so.1",
            "/usr/lib64/libclang.so",
            "/usr/lib64/libclang.so.1",
        ]
        .map(|path| (PathBuf::from(path), Ok(false)))
        .into()
    }

    #[test]
    fn explicit_library_directory_has_priority_on_each_platform() {
        for (platform, name) in [
            (Platform::Windows, "libclang.dll"),
            (Platform::Linux, "libclang.so"),
            (Platform::MacOs, "libclang.dylib"),
        ] {
            let mut environment = environment(platform);
            environment.libclang_path = Some("custom LLVM".into());
            environment.path = vec!["custom LLVM".into()];
            check_scripted(&environment, vec![(Path::new("custom LLVM").join(name), Ok(true))], vec![]).unwrap();
        }
    }

    #[test]
    fn absent_override_falls_back_to_windows_scoop_and_normalizes_path_entries() {
        let mut environment = environment(Platform::Windows);
        environment.libclang_path = Some("absent".into());
        environment.user_profile = Some(r"C:\Users\developer".into());
        environment.path = vec![r"c:/users/developer/scoop/apps/llvm/current/bin/".into()];
        check_scripted(
            &environment,
            vec![
                (Path::new("absent").join("libclang.dll"), Err(io::ErrorKind::NotFound.into())),
                (Path::new(r"C:\Program Files\LLVM\bin").join("libclang.dll"), Ok(false)),
                (
                    Path::new(r"C:\Users\developer")
                        .join(r"scoop\apps\llvm\current\bin")
                        .join("libclang.dll"),
                    Ok(true),
                ),
            ],
            vec![],
        )
        .unwrap();
    }

    #[test]
    fn windows_library_without_matching_path_reports_the_fix() {
        let mut environment = environment(Platform::Windows);
        environment.libclang_path = Some(r"C:\LLVM\bin".into());
        environment.path = vec![r"C:\LLVM\bin-extra".into()];
        let error = check_scripted(
            &environment,
            vec![(Path::new(r"C:\LLVM\bin").join("libclang.dll"), Ok(true))],
            vec![],
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains(r"libclang.dll exists in `C:\LLVM\bin` but that directory is not on PATH"));
        assert!(message.contains(r"Set LIBCLANG_PATH to `C:\LLVM\bin` and add that directory to PATH"));
    }

    #[test]
    fn macos_probes_both_homebrew_prefixes_without_requiring_path() {
        check_scripted(
            &environment(Platform::MacOs),
            vec![
                ("/usr/local/opt/llvm/lib/libclang.dylib".into(), Ok(false)),
                ("/opt/homebrew/opt/llvm/lib/libclang.dylib".into(), Ok(true)),
            ],
            vec![],
        )
        .unwrap();
    }

    #[test]
    fn linux_accepts_versioned_soname_without_requiring_path() {
        check_scripted(
            &environment(Platform::Linux),
            vec![
                ("/usr/lib/x86_64-linux-gnu/libclang.so".into(), Ok(false)),
                ("/usr/lib/x86_64-linux-gnu/libclang.so.1".into(), Ok(true)),
            ],
            vec![],
        )
        .unwrap();
    }

    #[test]
    fn linux_fallback_sorts_matching_names_and_rejects_non_files() {
        let mut files = absent_linux_files();
        files.extend([
            ("/usr/lib/libclang-17.so.1".into(), Ok(false)),
            ("/usr/lib/libclang-18.so.1".into(), Ok(true)),
        ]);
        check_scripted(
            &environment(Platform::Linux),
            files,
            vec![(
                "/usr/lib".into(),
                Ok(["libclang-18.so.1", "libclang.a", "other.so", "libclang-17.so.1"]
                    .map(OsString::from)
                    .into()),
            )],
        )
        .unwrap();
    }

    #[test]
    fn linux_fallback_skips_absent_directories_and_uses_later_locations() {
        let mut files = absent_linux_files();
        files.push(("/usr/lib64/libclang-18.so.1".into(), Ok(true)));
        check_scripted(
            &environment(Platform::Linux),
            files,
            vec![
                ("/usr/lib".into(), Err(io::ErrorKind::NotFound.into())),
                ("/usr/lib64".into(), Ok(vec!["libclang-18.so.1".into()])),
            ],
        )
        .unwrap();
    }

    #[test]
    fn missing_libraries_report_platform_specific_installation_guidance() {
        let cases = [
            (
                Platform::Windows,
                vec![(Path::new(r"C:\Program Files\LLVM\bin").join("libclang.dll"), Ok(false))],
                vec![],
                "scoop install llvm",
            ),
            (
                Platform::MacOs,
                vec![
                    ("/usr/local/opt/llvm/lib/libclang.dylib".into(), Ok(false)),
                    ("/opt/homebrew/opt/llvm/lib/libclang.dylib".into(), Ok(false)),
                ],
                vec![],
                "brew install llvm",
            ),
            (
                Platform::Linux,
                absent_linux_files(),
                ["/usr/lib", "/usr/lib64", "/usr/lib/x86_64-linux-gnu", "/usr/lib/aarch64-linux-gnu"]
                    .map(|path| (PathBuf::from(path), Ok(Vec::new())))
                    .into(),
                "apt-get install -y libclang-dev",
            ),
        ];
        for (platform, files, directories, hint) in cases {
            let error = check_scripted(&environment(platform), files, directories).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("missing source-build dependency"));
            assert!(message.contains(hint));
            assert!(message.contains("retry setup"));
        }
    }

    #[test]
    fn candidate_read_failure_preserves_cause_and_does_not_fall_back() {
        let mut environment = environment(Platform::Linux);
        environment.libclang_path = Some("inaccessible".into());
        let path = Path::new("inaccessible").join("libclang.so");
        let error = check_scripted(
            &environment,
            vec![(
                path.clone(),
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "scripted refusal")),
            )],
            vec![],
        )
        .unwrap_err();
        assert_eq!(error.find_source::<io::Error>().unwrap().kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error
                .to_string()
                .contains(&format!("failed to inspect libclang candidate `{}`", path.display()))
        );
    }

    #[test]
    fn directory_read_failure_is_not_reported_as_a_missing_library() {
        let error = check_scripted(
            &environment(Platform::Linux),
            absent_linux_files(),
            vec![(
                "/usr/lib".into(),
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "scripted directory refusal")),
            )],
        )
        .unwrap_err();
        assert_eq!(error.find_source::<io::Error>().unwrap().kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("failed to search libclang directory `/usr/lib`"));
    }
}
