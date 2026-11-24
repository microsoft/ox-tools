// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

use ohno::{AppError, IntoAppError};

use super::{CommandOutput, Probe, Target};

pub(super) struct Native;

#[cfg_attr(coverage_nightly, coverage(off))]
impl Probe for Native {
    // #[gamma::skip(all, tag = "native-boundary", reason = "Resolves paths through the process working directory and Windows drive state; scripted path-resolution results test selection, comparison, and failure propagation without host state.")]
    fn absolute_path(&mut self, path: &Path) -> io::Result<PathBuf> {
        std::path::absolute(if path.as_os_str().is_empty() { Path::new(".") } else { path })
    }

    // #[gamma::skip(all, tag = "native-boundary", reason = "Traverses real directories and metadata through glob; discovery policy is tested with scripted paths, while this adapter requires provisioned filesystem state.")]
    fn glob(&mut self, pattern: &str, case_sensitive: bool) -> Result<Vec<PathBuf>, AppError> {
        let options = glob::MatchOptions {
            case_sensitive,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        let matches = glob::glob_with(pattern, options).into_app_err(format!("invalid libclang search pattern `{pattern}`"))?;
        let mut paths = Vec::new();
        for path in matches {
            let path = path.into_app_err(format!("failed to search libclang pattern `{pattern}`"))?;
            if !case_sensitive {
                match std::fs::metadata(&path) {
                    Ok(metadata) if metadata.is_dir() => {}
                    Ok(_) => continue,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error).into_app_err(format!("cannot inspect LLVM search directory `{}`", path.display())),
                }
            }
            paths.push(path);
        }
        Ok(paths)
    }

    // #[gamma::skip(all, tag = "native-boundary", reason = "Only maps native filesystem metadata to is_file; link policy is tested with scripted metadata outcomes.")]
    fn is_file(&mut self, path: &Path) -> io::Result<bool> {
        std::fs::metadata(path).map(|metadata| metadata.is_file())
    }

    // #[gamma::skip(all, tag = "native-boundary", reason = "Opens and reads an installed native library; format validation tests script byte ranges and read failures without creating files.")]
    fn read(&mut self, path: &Path, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    // #[gamma::skip(all, tag = "native-boundary", reason = "Only converts native process output to the probe result; orchestration tests supply command outputs without child processes.")]
    fn command(&mut self, executable: &OsStr, args: &[&str]) -> io::Result<CommandOutput> {
        let output = Command::new(executable).args(args).output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    // #[gamma::skip(all, tag = "native-boundary", reason = "Requires a host-compatible libclang and its native dependencies; loader failures and sequencing are tested through the injected probe.")]
    fn load_library(&mut self, path: &Path) -> Result<(), AppError> {
        // SAFETY: this deliberately loads the user-selected native LLVM library,
        // just as bindgen does. No symbol is called, and none outlives the library.
        let library = unsafe { libloading::Library::new(path) }.into_app_err("dynamic loader rejected libclang")?;
        // SAFETY: only symbol presence is checked; it is never called or dereferenced.
        unsafe { library.get::<*const ()>(b"clang_createIndex\0") }.into_app_err("library does not export clang_createIndex")?;
        Ok(())
    }

    // #[gamma::skip(all, tag = "native-boundary", reason = "cc discovers and launches installed native compilers and an archiver; portable tests inject tool-check outcomes rather than depend on native tools or child processes.")]
    fn check_tools(&mut self, target: &Target) -> Result<(), AppError> {
        let mut build = cc::Build::new();
        build
            .target(&target.triple)
            .host(&target.triple)
            .opt_level(0)
            .no_default_flags(true)
            .inherit_rustflags(false)
            .inherit_trim_paths(false)
            .cargo_metadata(false)
            .cargo_warnings(false);
        for (cpp, description) in [(false, "C compiler"), (true, "C++ compiler")] {
            build.cpp(cpp);
            let compiler = build.try_get_compiler().into_app_err(format!(
                "cannot discover {description}; install native development tools or configure {}",
                if cpp { "CXX" } else { "CC" }
            ))?;
            let mut command = compiler.to_command();
            command.arg(if compiler.is_like_msvc() { "/?" } else { "--version" });
            let output = command
                .output()
                .into_app_err(format!("cannot execute {description} `{}`", compiler.path().display()))?;
            if !output.status.success() {
                let details = format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return Err(AppError::new(format!(
                    "{description} `{}` failed its help/version probe: {}. Fix the compiler installation or its environment settings.",
                    compiler.path().display(),
                    details.trim()
                )));
            }
        }
        let mut archiver = build
            .try_get_archiver()
            .into_app_err("cannot discover native archiver; install native development tools or configure AR")?;
        archiver.arg(if target.msvc { "/?" } else { "--version" });
        let name = archiver.get_program().to_string_lossy().into_owned();
        // BSD archivers can return nonzero for --version; launching is the availability check.
        archiver.output().into_app_err(format!(
            "cannot execute native archiver `{name}`; install native development tools or configure AR"
        ))?;
        Ok(())
    }
}
