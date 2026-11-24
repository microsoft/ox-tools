// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod library;
mod native;

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use ohno::{AppError, IntoAppError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Windows,
    Linux,
    MacOs,
}

#[derive(Debug)]
struct Target {
    triple: String,
    platform: Platform,
    arch: String,
    pointer_width: u8,
    msvc: bool,
}

impl Target {
    fn parse(triple: &str) -> Result<Self, AppError> {
        let platform = if triple.contains("-windows-") {
            Platform::Windows
        } else if triple.contains("-linux-") {
            Platform::Linux
        } else if triple.ends_with("-darwin") {
            Platform::MacOs
        } else {
            return Err(AppError::new(format!(
                "unsupported native installation target `{triple}`; expected Windows, Linux, or macOS"
            )));
        };
        let arch = triple.split('-').next().unwrap_or_default();
        let pointer_width = if triple.ends_with("-gnux32") {
            32
        } else {
            match arch {
                "x86_64" | "aarch64" | "powerpc64" | "powerpc64le" | "s390x" | "riscv64gc" | "loongarch64" => 64,
                "i586" | "i686" | "arm" | "armv7" | "armv7a" | "thumbv7neon" | "powerpc" | "riscv32gc" => 32,
                _ => {
                    return Err(AppError::new(format!(
                        "unsupported architecture in native installation target `{triple}`"
                    )));
                }
            }
        };
        Ok(Self {
            triple: triple.to_owned(),
            platform,
            arch: arch.to_owned(),
            pointer_width,
            msvc: triple.ends_with("-msvc"),
        })
    }
}

#[derive(Debug)]
struct Environment {
    target: Target,
    libclang_path: Option<PathBuf>,
    llvm_config: OsString,
    ld_library_path: Vec<PathBuf>,
    path: Vec<PathBuf>,
}

#[derive(Debug)]
struct CommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

trait Probe {
    fn glob(&mut self, pattern: &str, case_sensitive: bool) -> Result<Vec<PathBuf>, AppError>;
    fn absolute_path(&mut self, path: &Path) -> io::Result<PathBuf>;
    fn is_file(&mut self, path: &Path) -> io::Result<bool>;
    fn read(&mut self, path: &Path, offset: u64, length: usize) -> io::Result<Vec<u8>>;
    fn command(&mut self, executable: &std::ffi::OsStr, args: &[&str]) -> io::Result<CommandOutput>;
    fn load_library(&mut self, path: &Path) -> Result<(), AppError>;
    fn check_tools(&mut self, target: &Target) -> Result<(), AppError>;
}

#[cfg_attr(coverage_nightly, coverage(off))]
// #[gamma::skip(all, tag = "native-boundary", reason = "Only supplies native environment access and the native adapter; check_environment tests the same orchestration with owned environment values and a scripted probe.")]
pub(super) fn check() -> Result<(), AppError> {
    check_environment(&mut native::Native, |name| std::env::var_os(name))
}

fn check_environment(probe: &mut impl Probe, mut variable: impl FnMut(&str) -> Option<OsString>) -> Result<(), AppError> {
    let output = probe
        .command(std::ffi::OsStr::new("rustc"), &["-vV"])
        // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "Only changes contextual prose; tests verify that the command failure and its typed cause propagate.")]
        .into_app_err("failed to run `rustc -vV` to identify the installation host")?;
    if !output.success {
        return Err(AppError::new(format!("`rustc -vV` failed: {}", output.stderr)));
    }
    let build_target = variable("CARGO_BUILD_TARGET");
    let build_target = match build_target {
        Some(target) if target == "host-tuple" => {
            let cargo = probe
                .command(std::ffi::OsStr::new("cargo"), &["-vV"])
                // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "Only changes command-failure context; the injected execution error and typed cause must still propagate.")]
                .into_app_err("failed to run `cargo -vV` to resolve CARGO_BUILD_TARGET=host-tuple")?;
            if !cargo.success {
                return Err(AppError::new(format!("`cargo -vV` failed: {}", cargo.stderr)));
            }
            // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "The tool label affects only missing-host prose; failed host parsing must still reject the alias.")]
            Some(OsString::from(version_host(&cargo.stdout, "cargo")?))
        }
        target => target,
    };
    let target = installation_target(&output.stdout, build_target.as_deref())?;
    let environment = Environment {
        target,
        libclang_path: variable("LIBCLANG_PATH").map(PathBuf::from),
        llvm_config: variable("LLVM_CONFIG_PATH").unwrap_or_else(|| "llvm-config".into()),
        ld_library_path: variable("LD_LIBRARY_PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default(),
        path: variable("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default(),
    };
    check_with(&environment, probe)
}

fn installation_target(rustc_output: &str, build_target: Option<&std::ffi::OsStr>) -> Result<Target, AppError> {
    // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, tag = "diagnostic", reason = "The tool label affects only missing-host prose; failed host parsing must still reject installation.")]
    let host = version_host(rustc_output, "rustc")?;
    if let Some(build_target) = build_target
        && build_target != host
    {
        return Err(AppError::new(format!(
            "CARGO_BUILD_TARGET `{}` differs from rustc host `{host}`; this check supports native cargo-spellcheck installations only. \
                 Unset CARGO_BUILD_TARGET before checking and installing.",
            build_target.to_string_lossy()
        )));
    }
    Target::parse(host)
}

fn version_host<'a>(output: &'a str, tool: &str) -> Result<&'a str, AppError> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or_else(|| AppError::new(format!("`{tool} -vV` did not report a host target")))
}

fn check_with(environment: &Environment, probe: &mut impl Probe) -> Result<(), AppError> {
    let library = find_libclang(environment, probe)?;
    library::check_link_requirements(environment, &library, probe)?;
    let library = probe
        .absolute_path(&library)
        .into_app_err(format!("cannot resolve selected libclang path `{}`", library.display()))?;
    probe.load_library(&library).into_app_err(format!(
        "cannot load libclang `{}` for rustc host `{}`; this helper runs as {}-bit {}. \
         If you changed toolchain architecture, reinstall cargo-spellcheck-helper for the active toolchain. \
         Otherwise install a matching LLVM distribution and make its dependent libraries discoverable",
        library.display(),
        environment.target.triple,
        usize::BITS,
        std::env::consts::ARCH
    ))?;
    probe.check_tools(&environment.target)
}

fn find_libclang(environment: &Environment, probe: &mut impl Probe) -> Result<PathBuf, AppError> {
    let mut candidates = Vec::new();
    let mut notes = Vec::new();
    let mut first_search_error = None;
    if let Some(path) = &environment.libclang_path {
        if let Some(parent) = path.parent() {
            let files = search_directory(environment, parent, probe)?;
            if files.contains(path) {
                candidates.push(path.clone());
            }
        }
        if candidates.is_empty() {
            candidates.extend(search_directory(environment, path, probe)?);
        }
    } else {
        if let Some(prefix) = optional_directory_command(probe, &environment.llvm_config, &["--prefix"], &mut notes) {
            for name in ["bin", "lib", "lib64"] {
                // #[gamma::skip(call_result.default, tag = "equivalent", reason = "default_after still executes this side-effecting call and replaces its unit result with unit.")]
                append_optional_paths(
                    search_directory(environment, &prefix.join(name), probe),
                    &mut candidates,
                    &mut notes,
                    &mut first_search_error,
                );
            }
        }
        if environment.target.platform == Platform::MacOs
            && let Some(prefix) = optional_directory_command(probe, std::ffi::OsStr::new("xcode-select"), &["--print-path"], &mut notes)
        {
            // #[gamma::skip(call_result.default, tag = "equivalent", reason = "default_after still executes this side-effecting call and replaces its unit result with unit.")]
            append_optional_paths(
                search_directory(environment, &prefix.join("Toolchains/XcodeDefault.xctoolchain/usr/lib"), probe),
                &mut candidates,
                &mut notes,
                &mut first_search_error,
            );
        }
        for directory in &environment.ld_library_path {
            // #[gamma::skip(call_result.default, tag = "equivalent", reason = "default_after still executes this side-effecting call and replaces its unit result with unit.")]
            append_optional_paths(
                search_directory(environment, directory, probe),
                &mut candidates,
                &mut notes,
                &mut first_search_error,
            );
        }
        for pattern in directory_patterns(&environment.target) {
            let mut directories = Vec::new();
            // #[gamma::skip(call_result.default, tag = "equivalent", reason = "default_after still executes this side-effecting call and replaces its unit result with unit.")]
            append_optional_paths(probe.glob(pattern, false), &mut directories, &mut notes, &mut first_search_error);
            for directory in directories {
                // #[gamma::skip(call_result.default, tag = "equivalent", reason = "default_after still executes this side-effecting call and replaces its unit result with unit.")]
                append_optional_paths(
                    search_directory(environment, &directory, probe),
                    &mut candidates,
                    &mut notes,
                    &mut first_search_error,
                );
            }
        }
    }
    let mut best: Option<(PathBuf, Vec<u32>)> = None;
    for candidate in candidates {
        match library::validate(&environment.target, &candidate, probe) {
            Ok(()) => {
                let version = library::version(&candidate);
                if best.as_ref().is_none_or(|(_, previous)| version > *previous) {
                    best = Some((candidate, version));
                }
            }
            Err(error) => notes.push(format!("{}: {error}", candidate.display())),
        }
    }
    if let Some((path, _)) = best {
        return Ok(path);
    }
    let override_hint = environment.libclang_path.as_ref().map_or(String::new(), |path| {
        format!(
            "\nLIBCLANG_PATH `{}` is authoritative; fix or unset it to search other installations.",
            path.display()
        )
    });
    // #[gamma::skip(cond.always_false, tag = "equivalent", reason = "With no notes this only adds an empty Discovery details heading to the already-failed result.")]
    let details = if notes.is_empty() {
        String::new()
    } else {
        format!("\nDiscovery details:\n{}", notes.join("\n"))
    };
    let message = format!(
        "No valid link-time libclang found for `{}`.{}{}\n{}",
        environment.target.triple,
        override_hint,
        details,
        installation_hint(environment.target.platform)
    );
    match first_search_error {
        Some(error) => Err(error).into_app_err(message),
        None => Err(AppError::new(message)),
    }
}

fn append_optional_paths(
    result: Result<Vec<PathBuf>, AppError>,
    paths: &mut Vec<PathBuf>,
    notes: &mut Vec<String>,
    first_error: &mut Option<AppError>,
) {
    match result {
        Ok(found) => paths.extend(found),
        Err(error) => {
            notes.push(error.to_string());
            if first_error.is_none() {
                *first_error = Some(error);
            }
        }
    }
}

fn search_directory(environment: &Environment, directory: &Path, probe: &mut impl Probe) -> Result<Vec<PathBuf>, AppError> {
    let mut found = search_files(environment.target.platform, directory, probe)?;
    if environment.target.platform == Platform::Windows
        && directory.ends_with("lib")
        && let Some(parent) = directory.parent()
    {
        found.extend(search_files(environment.target.platform, &parent.join("bin"), probe)?);
    }
    Ok(found)
}

fn search_files(platform: Platform, directory: &Path, probe: &mut impl Probe) -> Result<Vec<PathBuf>, AppError> {
    let names: &[&str] = match platform {
        Platform::Windows => &["clang.dll", "libclang.dll"],
        Platform::Linux => &["libclang.so", "libclang-*.so"],
        Platform::MacOs => &["libclang.dylib"],
    };
    let directory = directory
        .to_str()
        .ok_or_else(|| AppError::new(format!("libclang search directory is not valid Unicode: `{}`", directory.display())))?;
    let directory = glob::Pattern::escape(directory);
    let mut files = Vec::new();
    for name in names {
        let pattern = Path::new(&directory).join(name);
        let pattern = pattern
            .to_str()
            .expect("joining a Unicode directory and a Unicode library name preserves Unicode");
        for file in probe.glob(pattern, true)? {
            if !file.file_name().is_some_and(|name| name.to_string_lossy().contains("-cpp.")) {
                files.push(file);
            }
        }
    }
    Ok(files)
}

fn optional_directory_command(
    probe: &mut impl Probe,
    executable: &std::ffi::OsStr,
    args: &[&str],
    notes: &mut Vec<String>,
) -> Option<PathBuf> {
    let result = probe.command(executable, args);
    match result {
        Ok(output) if output.success => {
            if let Some(line) = output.stdout.lines().next().filter(|line| !line.is_empty()) {
                Some(PathBuf::from(line))
            } else {
                notes.push(format!("{} {} returned no directory", executable.to_string_lossy(), args.join(" ")));
                None
            }
        }
        Ok(output) => {
            notes.push(format!(
                "{} {} failed: {}",
                executable.to_string_lossy(),
                args.join(" "),
                output.stderr.trim()
            ));
            None
        }
        Err(error) => {
            notes.push(format!("{} {}: {error}", executable.to_string_lossy(), args.join(" ")));
            None
        }
    }
}

fn directory_patterns(target: &Target) -> &'static [&'static str] {
    match target.platform {
        Platform::Linux => &[
            "/usr/local/llvm*/lib*",
            "/usr/local/lib*/*/*",
            "/usr/local/lib*/*",
            "/usr/local/lib*",
            "/usr/lib*/*/*",
            "/usr/lib*/*",
            "/usr/lib*",
        ],
        Platform::MacOs => &[
            "/usr/local/opt/llvm*/lib/llvm*/lib",
            "/Library/Developer/CommandLineTools/usr/lib",
            "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib",
            "/usr/local/opt/llvm*/lib",
        ],
        Platform::Windows if target.msvc => &[
            r"C:\Users\*\scoop\apps\llvm\current\lib",
            r"C:\Program Files*\LLVM\lib",
            r"C:\LLVM\lib",
            r"C:\Program Files*\Microsoft Visual Studio\*\VC\Tools\Llvm\**\lib",
        ],
        Platform::Windows => &[
            r"C:\Users\*\scoop\apps\llvm\current\lib",
            r"C:\MSYS*\MinGW*\lib",
            r"C:\Program Files*\LLVM\lib",
            r"C:\LLVM\lib",
            r"C:\Program Files*\Microsoft Visual Studio\*\VC\Tools\Llvm\**\lib",
        ],
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
// #[gamma::skip(all, tag = "diagnostic", reason = "Only returns platform-specific installation prose; asserting copied constants would not test prerequisite behavior.")]
fn installation_hint(platform: Platform) -> &'static str {
    match platform {
        Platform::Windows => {
            "Install LLVM (`scoop install llvm` or `winget install LLVM.LLVM`) and Visual Studio C++ build tools. \
            Set LIBCLANG_PATH to the matching library file or directory and add its DLL directory to PATH."
        }
        Platform::Linux => {
            "Install a C/C++ development toolchain and libclang development libraries. \
            Ubuntu/Debian: `sudo apt-get install build-essential libclang-dev`; \
            Azure Linux/RHEL: install gcc, gcc-c++, binutils, and clang-devel. \
            Set LIBCLANG_PATH or LLVM_CONFIG_PATH for a nonstandard LLVM installation."
        }
        Platform::MacOs => {
            "Install Xcode command-line tools (`xcode-select --install`) and LLVM (`brew install llvm`). \
            Set LLVM_CONFIG_PATH to the installed llvm-config executable or LIBCLANG_PATH to its libclang.dylib."
        }
    }
}

#[cfg(test)]
#[path = "source_deps/tests.rs"]
mod tests;
