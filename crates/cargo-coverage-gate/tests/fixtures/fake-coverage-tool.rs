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
        "llvm-cov" => run_cov(&args),
        "rustc" => run_rustc(&args),
        other => Err(format!("unexpected fake tool name `{other}`")),
    }
}

fn run_cargo(args: &[std::ffi::OsString]) -> Result<(), String> {
    if let Some(expected) = env::var_os("FAKE_EXPECT_RUSTUP_TOOLCHAIN")
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
        let release = if env::var_os("FAKE_STABLE_CARGO").is_some() {
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
            .env_remove("RUSTUP_TOOLCHAIN")
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
        if env::var_os("FAKE_COMPILER_MESSAGE").is_some() {
            println!("fake compiler diagnostic");
        }
    }
    if args.first().is_some_and(|arg| arg == "llvm-cov") && args.iter().any(|arg| arg == "report") {
        if env::var_os("FAKE_REPORT_STDOUT").is_some() {
            println!("report-stdout");
        }
        if env::var_os("FAKE_NO_PROFILE").is_some() {
            return Err("no raw profiles found".to_owned());
        }
        if env::var_os("FAKE_REPORT_COMMAND_TOO_LONG").is_some() {
            let llvm_cov = env::var("LLVM_COV").map_err(|error| error.to_string())?;
            let object = env::var("FAKE_COVERAGE_OBJECT").map_err(|error| error.to_string())?;
            eprintln!(
                "error: failed to generate report: could not execute process `\"{llvm_cov}\" export -format=lcov -instr-profile=\"fake.profdata\" -object \"{object}\" -ignore-filename-regex \"UPSTREAM_DEFAULTS\"` (never executed): The filename or extension is too long. (os error 206)"
            );
            return Err("requested command-too-long report failure".to_owned());
        }
        if env::var_os("FAKE_NO_COVERAGE_DATA").is_some() {
            eprintln!("error: failed to load coverage: 'empty': no coverage data found");
            eprintln!("error: could not load coverage information");
            return Err("requested no-coverage-data report failure".to_owned());
        }
        let output =
            value_after(args, "--output-path").ok_or_else(|| "cargo llvm-cov report did not receive --output-path".to_owned())?;
        let contents = if env::var_os("FAKE_EMPTY_LCOV").is_some() {
            String::new()
        } else {
            fake_lcov()?
        };
        fs::write(output, contents).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn run_rustc(args: &[std::ffi::OsString]) -> Result<(), String> {
    if let Some(expected) = env::var_os("FAKE_EXPECT_RUSTUP_TOOLCHAIN")
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
        let release = if env::var_os("FAKE_STABLE_RUSTC").is_some() {
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

fn run_cov(args: &[std::ffi::OsString]) -> Result<(), String> {
    let response = args
        .iter()
        .find_map(|arg| arg.to_str()?.strip_prefix('@').map(PathBuf::from))
        .ok_or_else(|| "llvm-cov did not receive a response file".to_owned())?;
    let response_contents = fs::read_to_string(response).map_err(|error| error.to_string())?;
    let response_log = env::var_os("FAKE_RESPONSE_LOG").ok_or_else(|| "FAKE_RESPONSE_LOG is not set".to_owned())?;
    fs::write(response_log, response_contents).map_err(|error| error.to_string())?;

    print!("{}", fake_lcov()?);
    Ok(())
}

fn fake_lcov() -> Result<String, String> {
    use std::fmt::Write as _;

    let workspace = PathBuf::from(env::var_os("FAKE_WORKSPACE_ROOT").ok_or_else(|| "FAKE_WORKSPACE_ROOT is not set".to_owned())?);
    let hits = env::var("FAKE_LCOV_HITS").unwrap_or_else(|_| "1".to_owned());
    let covered = u8::from(hits != "0");
    let mut output = String::new();
    for package in ["alpha", "beta"] {
        let source = workspace.join(package).join("src").join("lib.rs");
        writeln!(output, "TN:").map_err(|error| error.to_string())?;
        writeln!(output, "SF:{}", source.display()).map_err(|error| error.to_string())?;
        writeln!(output, "DA:1,{hits}").map_err(|error| error.to_string())?;
        writeln!(output, "LF:1").map_err(|error| error.to_string())?;
        writeln!(output, "LH:{covered}").map_err(|error| error.to_string())?;
        writeln!(output, "end_of_record").map_err(|error| error.to_string())?;
    }
    Ok(output)
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
