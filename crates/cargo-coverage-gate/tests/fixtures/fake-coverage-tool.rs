// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fake coverage tool: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let name = executable
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "executable has no UTF-8 file stem".to_owned())?;
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    log(name, &args)?;

    match name {
        "cargo" => run_cargo(&args),
        "llvm-profdata" => run_profdata(&args),
        "llvm-cov" => run_cov(&args),
        "rustc" => run_rustc(&args),
        "rustup" => run_rustup(&args),
        other => Err(format!("unexpected fake tool name `{other}`")),
    }
}

fn run_rustup(args: &[std::ffi::OsString]) -> Result<(), String> {
    if env::var_os("FAKE_FAIL_RUSTUP").is_some() {
        return Err("requested rustup failure".to_owned());
    }
    if env::var_os("FAKE_EMPTY_RUSTUP_OUTPUT").is_some() {
        println!();
        return Ok(());
    }
    if args.len() != 4 || args[0] != "which" || args[1] != "--toolchain" {
        return Err(format!("unexpected rustup arguments: {args:?}"));
    }
    if let Some(expected) = env::var_os("FAKE_EXPECT_TOOLCHAIN")
        && args[2] != expected
    {
        return Err(format!(
            "expected rustup toolchain {}, got {}",
            expected.to_string_lossy(),
            args[2].to_string_lossy()
        ));
    }
    let program = args[3]
        .to_str()
        .ok_or_else(|| "rustup program name is not UTF-8".to_owned())?;
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let path = executable
        .parent()
        .ok_or_else(|| "fake rustup has no parent directory".to_owned())?
        .join(format!("{program}{}", env::consts::EXE_SUFFIX));
    println!("{}", path.display());
    Ok(())
}

fn run_cargo(args: &[std::ffi::OsString]) -> Result<(), String> {
    if let Some(expected) = env::var_os("FAKE_EXPECT_TOOLCHAIN")
        && env::var_os("RUSTUP_TOOLCHAIN").as_ref() != Some(&expected)
    {
        return Err(format!(
            "expected RUSTUP_TOOLCHAIN={}, got {}",
            expected.to_string_lossy(),
            env::var_os("RUSTUP_TOOLCHAIN")
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<unset>".to_owned())
        ));
    }
    if args == ["--version", "--verbose"] {
        if env::var_os("FAKE_FAIL_CARGO_VERSION").is_some() {
            return Err("requested cargo version failure".to_owned());
        }
        let release = if env::var_os("FAKE_STABLE_TOOLCHAIN").is_some() {
            "1.95.0"
        } else {
            "1.97.0-nightly"
        };
        println!("cargo {release}");
        println!("release: {release}");
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "llvm-cov") && args.iter().any(|arg| arg == "--version") {
        let version = env::var("FAKE_LLVM_COV_VERSION").unwrap_or_else(|_| "0.9.0".to_owned());
        println!("cargo-llvm-cov {version}");
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "metadata") {
        let real_cargo = env::var_os("FAKE_REAL_CARGO").ok_or_else(|| "FAKE_REAL_CARGO is not set".to_owned())?;
        let status = Command::new(real_cargo)
            .args(args)
            .status()
            .map_err(|error| error.to_string())?;
        if status.success() {
            return Ok(());
        }
        return Err(format!("real cargo metadata exited with {status}"));
    }

    if args.first().is_some_and(|arg| arg == "nextest") {
        if env::var_os("FAKE_FAIL_PLAIN_NEXTEST").is_some() {
            return Err("requested plain nextest failure".to_owned());
        }
        if env::var_os("FAKE_PLAIN_NEXTEST_STDOUT").is_some() {
            println!("plain-nextest-stdout");
        }
        return Ok(());
    }

    if args.first().is_some_and(|arg| arg == "llvm-cov") && args.iter().any(|arg| arg == "clean") {
        if env::var_os("FAKE_FAIL_CLEAN").is_some() {
            return Err("requested coverage clean failure".to_owned());
        }
        if env::var_os("FAKE_CLEAN_STDOUT").is_some() {
            println!("coverage-clean-stdout");
        }
        return Ok(());
    }

    if args.first().is_some_and(|arg| arg == "llvm-cov") && args.iter().any(|arg| arg == "nextest") {
        if env::var_os("FAKE_FAIL_NEXTEST").is_some() {
            return Err("requested nextest failure".to_owned());
        }
        let target_dir =
            PathBuf::from(env::var_os("CARGO_LLVM_COV_TARGET_DIR").ok_or_else(|| "coverage target directory is not set".to_owned())?);
        fs::create_dir_all(&target_dir).map_err(|error| error.to_string())?;
        if env::var_os("FAKE_NO_PROFILE").is_none() {
            fs::write(target_dir.join("fake.profraw"), b"profile").map_err(|error| error.to_string())?;
        }
        if let Ok(delay) = env::var("FAKE_NEXTEST_DELAY_MS") {
            let delay = delay.parse::<u64>().map_err(|error| error.to_string())?;
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }

        if env::var_os("FAKE_NEXTEST_TEXT").is_some() {
            println!("non-JSON nextest output");
        }
        if env::var_os("FAKE_NO_OBJECT").is_none() {
            let object = env::var("FAKE_COVERAGE_OBJECT").map_err(|error| error.to_string())?;
            println!(
                "{{\"reason\":\"compiler-artifact\",\"executable\":\"{}\"}}",
                json_escape(&object)
            );
        }
        println!("{{\"reason\":\"build-finished\",\"success\":true}}");
        break_directory("FAKE_BREAK_DIRECTORY_AFTER_NEXTEST")?;
    }
    Ok(())
}

fn run_rustc(args: &[std::ffi::OsString]) -> Result<(), String> {
    if let Some(expected) = env::var_os("FAKE_EXPECT_RUSTC_TOOLCHAIN")
        && env::var_os("RUSTUP_TOOLCHAIN").as_ref() != Some(&expected)
    {
        return Err(format!(
            "expected rustc RUSTUP_TOOLCHAIN={}, got {}",
            expected.to_string_lossy(),
            env::var_os("RUSTUP_TOOLCHAIN")
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<unset>".to_owned())
        ));
    }
    if env::var_os("FAKE_FAIL_RUSTC").is_some() {
        return Err("requested rustc failure".to_owned());
    }
    if env::var_os("FAKE_FAIL_TARGET_LIBDIR").is_some()
        && args.iter().any(|arg| arg == "target-libdir")
    {
        return Err("requested target-libdir failure".to_owned());
    }
    if env::var_os("FAKE_INVALID_RUSTC_OUTPUT").is_some() {
        std::io::stdout().write_all(&[0xFF]).map_err(|error| error.to_string())?;
        return Ok(());
    }
    if env::var_os("FAKE_EMPTY_TARGET_LIBDIR").is_some()
        && args.iter().any(|arg| arg == "target-libdir")
    {
        println!();
        return Ok(());
    }
    if args.iter().any(|arg| arg == "cfg") {
        println!("windows");
        println!("target_arch=\"x86_64\"");
        println!("target_os=\"windows\"");
        return Ok(());
    }
    if args.iter().any(|arg| arg == "-vV") {
        let release = if env::var_os("FAKE_STABLE_TOOLCHAIN").is_some()
            || env::var_os("FAKE_STABLE_RUSTC").is_some()
        {
            "1.95.0"
        } else {
            "1.97.0-nightly"
        };
        println!("release: {release}");
        println!("host: x86_64-pc-windows-msvc");
        return Ok(());
    }
    println!(
        "{}",
        env::var_os("FAKE_TARGET_LIBDIR")
            .ok_or_else(|| "FAKE_TARGET_LIBDIR is not set".to_owned())?
            .to_string_lossy()
    );
    Ok(())
}

fn run_profdata(args: &[std::ffi::OsString]) -> Result<(), String> {
    if env::var_os("FAKE_PROFDATA_STDOUT").is_some() {
        println!("profdata-stdout");
    }
    let output = value_after(args, "-o").ok_or_else(|| "llvm-profdata did not receive -o".to_owned())?;
    fs::write(output, b"profdata").map_err(|error| error.to_string())?;
    break_directory("FAKE_BREAK_DIRECTORY_AFTER_PROFDATA")
}

fn run_cov(args: &[std::ffi::OsString]) -> Result<(), String> {
    let response = args
        .iter()
        .find_map(|arg| arg.to_str()?.strip_prefix('@').map(PathBuf::from))
        .ok_or_else(|| "llvm-cov did not receive a response file".to_owned())?;
    let response_contents = fs::read_to_string(response).map_err(|error| error.to_string())?;
    let response_log = env::var_os("FAKE_RESPONSE_LOG").ok_or_else(|| "FAKE_RESPONSE_LOG is not set".to_owned())?;
    fs::write(response_log, response_contents).map_err(|error| error.to_string())?;

    if env::var_os("FAKE_FAIL_COV").is_some() {
        return Err("requested llvm-cov failure".to_owned());
    }
    if env::var_os("FAKE_EMPTY_LCOV").is_some() {
        return Ok(());
    }

    let workspace = PathBuf::from(env::var_os("FAKE_WORKSPACE_ROOT").ok_or_else(|| "FAKE_WORKSPACE_ROOT is not set".to_owned())?);
    for package in ["alpha", "beta"] {
        let source = workspace.join(package).join("src").join("lib.rs");
        println!("TN:");
        println!("SF:{}", source.display());
        println!("DA:1,1");
        println!("LF:1");
        println!("LH:1");
        println!("end_of_record");
    }
    Ok(())
}

fn value_after<'a>(args: &'a [std::ffi::OsString], expected: &str) -> Option<&'a Path> {
    args.windows(2)
        .find(|pair| pair[0] == expected)
        .map(|pair| Path::new(&pair[1]))
}

fn log(name: &str, args: &[std::ffi::OsString]) -> Result<(), String> {
    let Some(path) = env::var_os("FAKE_TOOL_LOG") else {
        return Ok(());
    };
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    let mut line = name.to_owned();
    for argument in args {
        line.push('\t');
        line.push_str(&argument.to_string_lossy());
    }
    if let Some(target_dir) = env::var_os("CARGO_LLVM_COV_TARGET_DIR") {
        line.push_str("\tCOVERAGE_TARGET=");
        line.push_str(&target_dir.to_string_lossy());
    }
    writeln!(log, "{line}").map_err(|error| error.to_string())
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn break_directory(variable: &str) -> Result<(), String> {
    let Some(path) = env::var_os(variable) else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    fs::remove_dir_all(&path).map_err(|error| error.to_string())?;
    fs::write(path, b"not a directory").map_err(|error| error.to_string())
}
