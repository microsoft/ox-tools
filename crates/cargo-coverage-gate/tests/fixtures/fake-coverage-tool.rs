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
        "rustc" => run_rustc(),
        other => Err(format!("unexpected fake tool name `{other}`")),
    }
}

fn run_cargo(args: &[std::ffi::OsString]) -> Result<(), String> {
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

    if args.iter().any(|arg| arg == "nextest") {
        if env::var_os("FAKE_FAIL_NEXTEST").is_some() {
            return Err("requested nextest failure".to_owned());
        }
        let target_dir =
            PathBuf::from(env::var_os("CARGO_LLVM_COV_TARGET_DIR").ok_or_else(|| "coverage target directory is not set".to_owned())?);
        fs::create_dir_all(&target_dir).map_err(|error| error.to_string())?;
        if env::var_os("FAKE_NO_PROFILE").is_none() {
            fs::write(target_dir.join("fake.profraw"), b"profile").map_err(|error| error.to_string())?;
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

fn run_rustc() -> Result<(), String> {
    if env::var_os("FAKE_FAIL_RUSTC").is_some() {
        return Err("requested rustc failure".to_owned());
    }
    if env::var_os("FAKE_INVALID_RUSTC_OUTPUT").is_some() {
        std::io::stdout().write_all(&[0xFF]).map_err(|error| error.to_string())?;
        return Ok(());
    }
    if env::var_os("FAKE_EMPTY_RUSTC_OUTPUT").is_some() {
        println!();
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
    write!(log, "{name}").map_err(|error| error.to_string())?;
    for argument in args {
        write!(log, "\t{}", argument.to_string_lossy()).map_err(|error| error.to_string())?;
    }
    writeln!(log).map_err(|error| error.to_string())
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
