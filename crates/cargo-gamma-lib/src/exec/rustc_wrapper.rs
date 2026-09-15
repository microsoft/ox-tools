// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

pub(crate) const CAPTURE_DIR_VAR: &str = "CARGO_GAMMA_RUSTC_CAPTURE_DIR";
pub(crate) const ORIGINAL_WRAPPER_VAR: &str = "CARGO_GAMMA_ORIGINAL_RUSTC_WRAPPER";

static NEXT_CAPTURE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RustcInvocation {
    pub(crate) crate_name: String,
    pub(crate) crate_types: Vec<String>,
    pub(crate) test: bool,
    pub(crate) source: Utf8PathBuf,
    pub(crate) out_dir: Utf8PathBuf,
    pub(crate) extra_filename: String,
    pub(crate) externs: Vec<Utf8PathBuf>,
    pub(crate) opaque_extern: bool,
}

/// Runs cargo-gamma as Cargo's rustc wrapper when the private capture marker is present.
///
/// Returns `None` for an ordinary cargo-gamma invocation.
pub fn run_if_requested(args: impl IntoIterator<Item = OsString>) -> Option<ExitCode> {
    let directory = std::env::var_os(CAPTURE_DIR_VAR)?;
    let mut args = args.into_iter();
    let _self = args.next();
    let forwarded: Vec<OsString> = args.collect();
    let compiler = forwarded.first()?;

    let mut command = if let Some(wrapper) = std::env::var_os(ORIGINAL_WRAPPER_VAR).filter(|path| !path.is_empty()) {
        let mut command = Command::new(wrapper);
        let _ = command.args(&forwarded);
        command
    } else {
        let mut command = Command::new(compiler);
        let _ = command.args(&forwarded[1..]);
        command
    };

    let status = match command.status() {
        Ok(status) => status,
        Err(_) => return Some(ExitCode::FAILURE),
    };

    if status.success()
        && let Some(mut invocation) = parse_invocation(&forwarded[1..])
    {
        invocation.opaque_extern |= std::env::vars_os().any(|(name, _value)| name.to_string_lossy().starts_with("CARGO_BIN_EXE_"));

        if let Ok(bytes) = serde_json::to_vec(&invocation) {
            let directory = Utf8PathBuf::from(directory.to_string_lossy().into_owned());
            let _published = publish_capture(&directory, &bytes);
        }
    }

    Some(
        status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from),
    )
}

/// Starts one compiler-capture generation with no entries from an earlier build.
///
/// Capture data is an optimization: failure to clear or create the directory must not stop Cargo.
pub(crate) fn reset_capture_directory(directory: &camino::Utf8Path) {
    match fs::remove_dir_all(directory.as_std_path()) {
        Ok(()) => {}
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {}
        Err(_cause) => return,
    }

    let _created = fs::create_dir_all(directory.as_std_path());
}

/// Publishes a complete capture under a name unique within this wrapper process.
///
/// The reader never observes a partially written JSON entry: bytes are written and flushed under a
/// temporary name, then renamed into the capture set. A wrapper interrupted before the rename can
/// leave only the temporary entry, which the next build removes with [`reset_capture_directory`].
fn publish_capture(directory: &camino::Utf8Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::create_dir_all(directory.as_std_path())?;

    // #[gamma::skip(literal.int_increment, reason = "the counter is used only to make filenames unique; stepping by one or two preserves uniqueness and no ordering is consumed")]
    let sequence = NEXT_CAPTURE.fetch_add(1, Ordering::Relaxed);
    let stem = format!("{}-{sequence}", std::process::id());
    let temporary = directory.join(format!("{stem}.tmp"));
    let published = directory.join(format!("{stem}.json"));
    let mut file = OpenOptions::new().create_new(true).write(true).open(temporary.as_std_path())?;

    file.write_all(bytes)?;
    file.sync_data()?;
    drop(file);
    fs::rename(temporary.as_std_path(), published.as_std_path())
}

pub(crate) fn parse_invocation(args: &[OsString]) -> Option<RustcInvocation> {
    let mut crate_name = None;
    let mut crate_types = Vec::new();
    let mut test = false;
    let mut source = None;
    let mut out_dir = None;
    let mut extra_filename = String::new();
    let mut externs = Vec::new();
    let mut opaque_extern = false;
    let mut index = 0;

    while index < args.len() {
        let argument = args[index].to_string_lossy();
        let separate = |name: &str| argument == name && index + 1 < args.len();

        if separate("--crate-name") {
            crate_name = Some(args[index + 1].to_string_lossy().into_owned());
            // #[gamma::skip(stmt.delete_assign, literal.int_to_zero, reason = "not consuming the split option and its value retries the same argument forever and is observed only as a timeout")]
            index += 2;
        } else if let Some(value) = argument.strip_prefix("--crate-name=") {
            crate_name = Some(value.to_owned());
            // #[gamma::skip(stmt.delete_assign, literal.int_decrement, reason = "not consuming the joined option retries the same argument forever and is observed only as a timeout")]
            index += 1;
        } else if separate("--crate-type") {
            crate_types.extend(args[index + 1].to_string_lossy().split(',').map(str::to_owned));
            // #[gamma::skip(assign.add_to_sub, stmt.delete_assign, literal.int_to_zero, reason = "moving backward or not consuming the split option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 2;
        } else if let Some(value) = argument.strip_prefix("--crate-type=") {
            crate_types.extend(value.split(',').map(str::to_owned));
            // #[gamma::skip(assign.add_to_sub, stmt.delete_assign, literal.int_decrement, reason = "moving backward or not consuming the joined option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 1;
        } else if argument == "--test" {
            test = true;
            index += 1;
        } else if separate("--out-dir") {
            out_dir = utf8(&args[index + 1]);
            // #[gamma::skip(assign.add_to_sub, stmt.delete_assign, literal.int_to_zero, reason = "moving backward or not consuming the split option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 2;
        } else if let Some(value) = argument.strip_prefix("--out-dir=") {
            out_dir = Some(Utf8PathBuf::from(value));
            // #[gamma::skip(assign.add_to_sub, stmt.delete_assign, literal.int_decrement, reason = "moving backward or not consuming the joined option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 1;
        } else if separate("-C") {
            if let Some(value) = args[index + 1].to_str().and_then(|value| value.strip_prefix("extra-filename=")) {
                extra_filename = value.to_owned();
            }
            // #[gamma::skip(assign.add_to_sub, stmt.delete_assign, reason = "moving backward or not consuming the split option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 2;
        } else if let Some(value) = argument.strip_prefix("-Cextra-filename=") {
            extra_filename = value.to_owned();
            // #[gamma::skip(stmt.delete_assign, literal.int_decrement, reason = "not consuming the joined option retries the same argument forever and is observed only as a timeout")]
            index += 1;
        } else if separate("--extern") {
            parse_extern(&args[index + 1], &mut externs, &mut opaque_extern);
            // #[gamma::skip(assign.add_to_sub, stmt.delete_assign, literal.int_to_zero, reason = "moving backward or not consuming the split option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 2;
        } else if let Some(value) = argument.strip_prefix("--extern=") {
            parse_extern(OsStr::new(value), &mut externs, &mut opaque_extern);
            // #[gamma::skip(assign.add_to_sub, stmt.delete_assign, literal.int_decrement, reason = "moving backward or not consuming the joined option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 1;
        } else {
            if !argument.starts_with('-') && argument.ends_with(".rs") && source.is_none() {
                source = utf8(&args[index]);
            }
            // #[gamma::skip(stmt.delete_assign, literal.int_decrement, reason = "not advancing past an unrecognized argument retries it forever and is observed only as a timeout")]
            index += 1;
        }
    }

    Some(RustcInvocation {
        crate_name: crate_name?,
        crate_types,
        test,
        source: source?,
        out_dir: out_dir?,
        extra_filename,
        externs,
        opaque_extern,
    })
}

fn parse_extern(value: &OsStr, externs: &mut Vec<Utf8PathBuf>, opaque: &mut bool) {
    let value = value.to_string_lossy();
    let Some((_name, path)) = value.split_once('=') else {
        *opaque = true;
        return;
    };

    externs.push(Utf8PathBuf::from(path));
}

fn utf8(value: &OsStr) -> Option<Utf8PathBuf> {
    value.to_str().map(Utf8PathBuf::from)
}

pub(crate) fn wrapper_path() -> Option<Utf8PathBuf> {
    let path = Utf8PathBuf::from_path_buf(std::env::current_exe().ok()?).ok()?;
    let stem = path.file_stem()?;

    (stem == "cargo-gamma").then_some(path)
}

#[cfg(test)]
mod tests {
    use std::env;

    use camino::Utf8Path;

    use super::*;

    const CHILD_SCENARIO: &str = "CARGO_GAMMA_RUSTC_WRAPPER_TEST";
    const TEST_COMPILER: &str = "CARGO_GAMMA_RUSTC_WRAPPER_COMPILER";

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_split_and_joined_rustc_options_without_mistaking_option_values_for_source() {
        let parsed = parse_invocation(&args(&[
            "--crate-name",
            "linked",
            "--cfg",
            "feature=\"a.rs\"",
            "--crate-type=lib,rlib",
            "src/lib.rs",
            "--test",
            "--out-dir",
            "target/debug/deps",
            "-Cextra-filename=-abc",
            "--extern",
            "dep=target/debug/deps/libdep-def.rlib",
        ]))
        .expect("complete invocation");

        assert_eq!(parsed.crate_name, "linked");
        assert_eq!(parsed.crate_types, ["lib", "rlib"]);
        assert_eq!(parsed.source, Utf8PathBuf::from("src/lib.rs"));
        assert_eq!(parsed.out_dir, Utf8PathBuf::from("target/debug/deps"));
        assert_eq!(parsed.extra_filename, "-abc");
        assert_eq!(parsed.externs, [Utf8PathBuf::from("target/debug/deps/libdep-def.rlib")]);
        assert!(parsed.test);
        assert!(!parsed.opaque_extern);
    }

    #[test]
    fn incomplete_invocations_and_pathless_externs_fail_open() {
        assert!(parse_invocation(&args(&["--crate-name", "x", "src/lib.rs"])).is_none());
        assert!(parse_invocation(&args(&["--crate-name"])).is_none());
        assert!(parse_invocation(&args(&["--out-dir"])).is_none());
        assert!(parse_invocation(&args(&["--extern"])).is_none());

        let parsed = parse_invocation(&args(&["--crate-name=x", "src/lib.rs", "--out-dir=target", "--extern=opaque"]))
            .expect("identity is otherwise complete");

        assert!(parsed.opaque_extern);
        assert!(parsed.externs.is_empty());
    }

    #[test]
    fn parses_split_codegen_options() {
        let parsed = parse_invocation(&args(&[
            "--crate-name",
            "split",
            "--crate-type",
            "lib,cdylib",
            "src/lib.rs",
            "--out-dir",
            "target",
            "-C",
            "extra-filename=-split",
        ]))
        .expect("complete invocation");

        assert_eq!(parsed.crate_types, ["lib", "cdylib"]);
        assert_eq!(parsed.extra_filename, "-split");
    }

    #[test]
    fn every_joined_option_and_source_guard_is_exercised_independently() {
        let parsed = parse_invocation(&args(&[
            "-not-source.rs",
            "--crate-name=joined",
            "--crate-type=lib,proc-macro",
            "--out-dir=target/joined",
            "-Cextra-filename=-joined",
            "--extern=dep=target/joined/libdep.rlib",
            "src/first.rs",
            "src/ignored.rs",
        ]))
        .expect("complete joined invocation");

        assert_eq!(parsed.crate_name, "joined");
        assert_eq!(parsed.crate_types, ["lib", "proc-macro"]);
        assert_eq!(parsed.out_dir, Utf8Path::new("target/joined"));
        assert_eq!(parsed.extra_filename, "-joined");
        assert_eq!(parsed.externs, [Utf8PathBuf::from("target/joined/libdep.rlib")]);
        assert_eq!(parsed.source, Utf8Path::new("src/first.rs"));
        assert!(!parsed.test);
        assert!(!parsed.opaque_extern);
    }

    #[test]
    fn reset_accepts_a_capture_directory_that_does_not_exist() {
        let directory = tempfile::tempdir().expect("test directory");
        let captures = Utf8PathBuf::from_path_buf(directory.path().join("missing")).expect("UTF-8 capture path");

        reset_capture_directory(&captures);

        assert!(captures.is_dir());
        assert_eq!(fs::read_dir(&captures).expect("capture directory").count(), 0);
    }

    #[test]
    fn a_test_executable_is_not_the_production_wrapper() {
        assert_eq!(wrapper_path(), None);
    }

    #[test]
    fn ordinary_invocations_do_not_enter_wrapper_mode() {
        if env::var_os(CAPTURE_DIR_VAR).is_none() {
            assert_eq!(run_if_requested(args(&["cargo-gamma"])), None);
        }
    }

    #[test]
    fn wrapper_child_helper() {
        let Ok(scenario) = env::var(CHILD_SCENARIO) else {
            return;
        };

        let invocation = match scenario.as_str() {
            "direct" => {
                let compiler = env::var(TEST_COMPILER).expect("test compiler");
                vec![
                    OsString::from("cargo-gamma"),
                    OsString::from(compiler),
                    OsString::from("--crate-name=wrapped"),
                    OsString::from("--crate-type=lib"),
                    OsString::from("src/lib.rs"),
                    OsString::from("--out-dir=target"),
                    OsString::from("--extern=opaque"),
                ]
            }
            "original" => args(&[
                "cargo-gamma",
                "ignored-rustc",
                "--crate-name=chained",
                "src/lib.rs",
                "--out-dir=target",
            ]),
            "missing-compiler" => args(&["cargo-gamma"]),
            "spawn-failure" => args(&["cargo-gamma", "definitely-not-a-real-rustc-wrapper-command"]),
            other => panic!("unknown wrapper child scenario `{other}`"),
        };

        let outcome = run_if_requested(invocation);
        println!("wrapper-present={}", outcome.is_some());
        println!("wrapper-success={}", outcome == Some(ExitCode::SUCCESS));
    }

    #[test]
    fn wrapper_mode_forwards_records_and_reports_failures() {
        let directory = tempfile::tempdir().expect("capture directory");
        let captures = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 capture path");
        let compiler = captures.join("compiler.cmd");
        fs::write(&compiler, "@exit /b 0\r\n").expect("compiler script");

        let direct = run_wrapper_child("direct", &captures, Some(compiler.as_str()), None, true);
        assert!(direct.contains("wrapper-success=true"), "{direct}");
        let recorded = read_only_capture(&captures);
        assert_eq!(recorded.crate_name, "wrapped");
        assert_eq!(recorded.source, Utf8PathBuf::from("src/lib.rs"));
        assert!(recorded.opaque_extern, "a pathless extern makes dependency reach opaque");

        fs::remove_file(capture_path(&captures)).expect("remove first capture");
        let script = captures.join("original.cmd");
        fs::write(&script, "@exit /b 0\r\n").expect("wrapper script");
        let chained = run_wrapper_child("original", &captures, None, Some(script.as_str()), false);
        assert!(chained.contains("wrapper-success=true"), "{chained}");
        assert_eq!(read_only_capture(&captures).crate_name, "chained");

        let missing = run_wrapper_child("missing-compiler", &captures, None, None, false);
        assert!(missing.contains("wrapper-present=false"), "{missing}");

        let failed = run_wrapper_child("spawn-failure", &captures, None, None, false);
        assert!(failed.contains("wrapper-present=true"), "{failed}");
        assert!(failed.contains("wrapper-success=false"), "{failed}");
    }

    #[test]
    fn a_new_build_recovers_from_interrupted_and_truncated_capture_entries() {
        let directory = tempfile::tempdir().expect("capture directory");
        let captures = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 capture path");
        fs::write(captures.join("interrupted.tmp"), br#"{"crate_name":"partial""#).expect("interrupted temporary entry");
        fs::write(captures.join("reused-pid.json"), br#"{"crate_name":"truncated""#).expect("truncated published entry");

        reset_capture_directory(&captures);

        assert_eq!(fs::read_dir(&captures).expect("reset capture directory").count(), 0);
        let invocation = RustcInvocation {
            crate_name: "recovered".to_owned(),
            crate_types: vec!["lib".to_owned()],
            test: false,
            source: Utf8PathBuf::from("src/lib.rs"),
            out_dir: Utf8PathBuf::from("target"),
            extra_filename: "-recovered".to_owned(),
            externs: Vec::new(),
            opaque_extern: false,
        };
        let bytes = serde_json::to_vec(&invocation).expect("capture JSON");

        publish_capture(&captures, &bytes).expect("complete capture publication");

        assert_eq!(read_only_capture(&captures), invocation);
        assert!(
            fs::read_dir(&captures)
                .expect("capture directory")
                .filter_map(core::result::Result::ok)
                .all(|entry| entry.path().extension().is_some_and(|extension| extension == "json")),
            "no temporary entry is published after a successful write"
        );
    }

    #[test]
    fn repeated_wrapper_publications_do_not_collide_on_the_process_id() {
        let directory = tempfile::tempdir().expect("capture directory");
        let captures = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 capture path");
        let first = br#"{"crate_name":"first"}"#;
        let second = br#"{"crate_name":"second"}"#;

        publish_capture(&captures, first).expect("first publication");
        publish_capture(&captures, second).expect("second publication");

        let mut contents: Vec<Vec<u8>> = fs::read_dir(&captures)
            .expect("capture directory")
            .map(|entry| fs::read(entry.expect("capture entry").path()).expect("capture bytes"))
            .collect();
        contents.sort();
        assert_eq!(contents, [first.to_vec(), second.to_vec()]);
    }

    #[test]
    fn capture_storage_failures_do_not_change_the_compiler_result() {
        let directory = tempfile::tempdir().expect("test directory");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 test path");
        let captures = root.join("not-a-directory");
        let compiler = root.join("compiler.cmd");
        fs::write(&captures, "keep").expect("capture blocker");
        fs::write(&compiler, "@exit /b 0\r\n").expect("compiler script");

        let output = run_wrapper_child("direct", &captures, Some(compiler.as_str()), None, false);

        assert!(output.contains("wrapper-success=true"), "{output}");
        assert_eq!(fs::read_to_string(&captures).expect("blocker remains"), "keep");
    }

    fn run_wrapper_child(
        scenario: &str,
        captures: &Utf8Path,
        compiler: Option<&str>,
        original: Option<&str>,
        opaque_binary: bool,
    ) -> String {
        let executable = env::current_exe().expect("test executable");
        let module = module_path!();
        let relative = module.split_once("::").map_or(module, |(_crate_name, rest)| rest);
        let test = format!("{relative}::wrapper_child_helper");
        let mut command = Command::new(executable);
        let _ = command
            .args([test.as_str(), "--exact", "--nocapture"])
            .env(CHILD_SCENARIO, scenario)
            .env(CAPTURE_DIR_VAR, captures);

        if let Some(compiler) = compiler {
            let _ = command.env(TEST_COMPILER, compiler);
        }
        if let Some(original) = original {
            let _ = command.env(ORIGINAL_WRAPPER_VAR, original);
        } else {
            let _ = command.env_remove(ORIGINAL_WRAPPER_VAR);
        }
        if opaque_binary {
            let _ = command.env("CARGO_BIN_EXE_fixture", "fixture");
        }

        let output = command.output().expect("wrapper child runs");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).expect("child output is UTF-8")
    }

    fn capture_path(captures: &Utf8Path) -> Utf8PathBuf {
        fs::read_dir(captures)
            .expect("capture directory")
            .filter_map(core::result::Result::ok)
            .map(|entry| Utf8PathBuf::from_path_buf(entry.path()).expect("UTF-8 capture path"))
            .find(|path| path.extension() == Some("json"))
            .expect("recorded invocation")
    }

    fn read_only_capture(captures: &Utf8Path) -> RustcInvocation {
        serde_json::from_slice(&fs::read(capture_path(captures)).expect("capture bytes")).expect("recorded invocation")
    }
}
