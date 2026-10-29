// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
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
///
/// Coverage excludes this process-boundary adapter; forwarding and capture policy are tested with
/// scripted process and environment collaborators.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn run_if_requested(args: impl IntoIterator<Item = OsString>) -> Option<ExitCode> {
    run_if_requested_with(
        args,
        wrapper_environment,
        || std::env::vars_os().map(|(name, _value)| name),
        compiler_status,
        publish_capture,
    )
}

/// Replaces compiler execution and supplies its exit status without starting a child.
#[cfg(test)]
type CompilerObserver = Box<dyn FnMut(&Command) -> std::io::Result<Option<i32>>>;

#[cfg(test)]
thread_local! {
    static WRAPPER_ENVIRONMENT: std::cell::RefCell<Option<crate::HashMap<String, OsString>>> = const { std::cell::RefCell::new(None) };
    static COMPILER_OBSERVER: std::cell::RefCell<Option<CompilerObserver>> = const { std::cell::RefCell::new(None) };
}

/// An installed map completely replaces named lookups, but not environment-name enumeration.
fn wrapper_environment(name: &str) -> Option<OsString> {
    #[cfg(test)]
    if let Some(value) = WRAPPER_ENVIRONMENT.with_borrow(|slot| slot.as_ref().map(|environment| environment.get(name).cloned())) {
        return value;
    }
    std::env::var_os(name)
}

fn compiler_status(command: &mut Command) -> std::io::Result<Option<i32>> {
    #[cfg(test)]
    if let Some(status) = COMPILER_OBSERVER.with_borrow_mut(|slot| slot.as_mut().map(|observer| observer(command))) {
        return status;
    }
    command.status().map(|status| status.code())
}

fn run_if_requested_with<I: IntoIterator<Item = OsString>>(
    args: impl IntoIterator<Item = OsString>,
    mut environment: impl FnMut(&str) -> Option<OsString>,
    binary_environment: impl FnOnce() -> I,
    run: impl FnOnce(&mut Command) -> std::io::Result<Option<i32>>,
    publish: impl FnOnce(&camino::Utf8Path, &[u8]) -> std::io::Result<()>,
) -> Option<ExitCode> {
    let directory = environment(CAPTURE_DIR_VAR).filter(|directory| !directory.is_empty())?;
    let mut args = args.into_iter();
    let _self = args.next();
    let forwarded: Vec<OsString> = args.collect();
    let compiler = forwarded.first()?;

    let mut command = if let Some(wrapper) = environment(ORIGINAL_WRAPPER_VAR).filter(|path| !path.is_empty()) {
        let mut command = Command::new(wrapper);
        let _ = command.args(&forwarded);
        command
    } else {
        let mut command = Command::new(compiler);
        let _ = command.args(&forwarded[1..]);
        command
    };

    let Ok(code) = run(&mut command) else {
        return Some(ExitCode::FAILURE);
    };

    if code == Some(0)
        && let Some(mut invocation) = parse_invocation(&forwarded[1..])
    {
        invocation.opaque_extern |= binary_environment()
            .into_iter()
            .any(|name| name.to_string_lossy().starts_with("CARGO_BIN_EXE_"));

        if let Ok(bytes) = serde_json::to_vec(&invocation) {
            let directory = Utf8PathBuf::from(directory.to_string_lossy().into_owned());
            let _published = publish(&directory, &bytes);
        }
    }

    Some(
        code.and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from),
    )
}

/// Starts one compiler-capture generation with no entries from an earlier build.
///
/// Capture data is an optimization: failure to clear or create the directory must not stop Cargo.
#[must_use]
pub(crate) fn reset_capture_directory(directory: &camino::Utf8Path) -> bool {
    match fs::remove_dir_all(directory.as_std_path()) {
        Ok(()) => {}
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {}
        Err(_cause) => return false,
    }

    fs::create_dir_all(directory.as_std_path()).is_ok()
}

/// Publishes a complete capture without replacing an entry from another wrapper process.
///
/// The reader never observes a partially written JSON entry: bytes are written and flushed under a
/// temporary name, then hard-linked into the capture set with exclusive destination semantics. A
/// wrapper interrupted before the link can
/// leave only the temporary entry, which the next build removes with [`reset_capture_directory`].
fn publish_capture(directory: &camino::Utf8Path, bytes: &[u8]) -> std::io::Result<()> {
    publish_capture_with(directory, bytes, next_capture_stem)
}

// Exclusive creation, flushing, hard-link publication, and cleanup are filesystem race/failure
// boundaries covered by publication-specific tests rather than deterministic line coverage.
#[cfg_attr(coverage_nightly, coverage(off))]
fn publish_capture_with(directory: &camino::Utf8Path, bytes: &[u8], next_stem: impl FnMut() -> String) -> std::io::Result<()> {
    publish_capture_with_io(directory, bytes, next_stem, Write::write_all, fs::File::sync_data, |from, to| {
        fs::hard_link(from, to)
    })
}

fn publish_capture_with_io(
    directory: &camino::Utf8Path,
    bytes: &[u8],
    mut next_stem: impl FnMut() -> String,
    write: impl FnOnce(&mut fs::File, &[u8]) -> std::io::Result<()>,
    flush: impl FnOnce(&fs::File) -> std::io::Result<()>,
    mut link: impl FnMut(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    fs::create_dir_all(directory.as_std_path())?;

    let (mut stem, temporary, mut file) = loop {
        let stem = next_stem();
        let temporary = directory.join(format!("{stem}.tmp"));
        match OpenOptions::new().create_new(true).write(true).open(temporary.as_std_path()) {
            Ok(file) => break (stem, temporary, file),
            Err(cause) if cause.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(cause) => return Err(cause),
        }
    };

    write(&mut file, bytes)?;
    flush(&file)?;
    drop(file);

    loop {
        let published = directory.join(format!("{stem}.json"));
        match link(temporary.as_std_path(), published.as_std_path()) {
            Ok(()) => break,
            Err(cause) if cause.kind() == std::io::ErrorKind::AlreadyExists => stem = next_stem(),
            Err(cause) => return Err(cause),
        }
    }

    fs::remove_file(temporary.as_std_path())
}

// #[gamma::skip(fn_value.xyzzy_string, tag = "timeout", reason = "a fixed capture filename repeatedly collides with existing captures in the campaign")]
fn next_capture_stem() -> String {
    // #[gamma::skip(literal.int_increment, reason = "the counter is used only to propose filenames; stepping by one or two preserves progress and no ordering is consumed")]
    // #[gamma::skip(literal.int_decrement, tag = "timeout", reason = "a zero counter step repeatedly proposes an already occupied capture filename")]
    let sequence = NEXT_CAPTURE.fetch_add(1, Ordering::Relaxed);
    format!("{}-{sequence}", std::process::id())
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
            // #[gamma::skip(all, reason = "not consuming the split option and its value retries the same argument forever and is observed only as a timeout")]
            index += 2;
        } else if let Some(value) = argument.strip_prefix("--crate-name=") {
            crate_name = Some(value.to_owned());
            // #[gamma::skip(all, reason = "not consuming the joined option retries the same argument forever and is observed only as a timeout")]
            index += 1;
        } else if separate("--crate-type") {
            crate_types.extend(args[index + 1].to_string_lossy().split(',').map(str::to_owned));
            // #[gamma::skip(all, reason = "moving backward or not consuming the split option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 2;
        } else if let Some(value) = argument.strip_prefix("--crate-type=") {
            crate_types.extend(value.split(',').map(str::to_owned));
            // #[gamma::skip(all, reason = "moving backward or not consuming the joined option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 1;
        } else if argument == "--test" {
            test = true;
            // #[gamma::skip(assign.add_to_sub, stmt.delete_assign, literal.int_decrement, tag = "timeout", reason = "the test-option scan must consume its argument to make progress")]
            index += 1;
        } else if separate("--out-dir") {
            out_dir = utf8(&args[index + 1]);
            // #[gamma::skip(all, reason = "moving backward or not consuming the split option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 2;
        } else if let Some(value) = argument.strip_prefix("--out-dir=") {
            out_dir = Some(Utf8PathBuf::from(value));
            // #[gamma::skip(all, reason = "moving backward or not consuming the joined option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 1;
        } else if separate("-C") {
            if let Some(value) = args[index + 1].to_str().and_then(|value| value.strip_prefix("extra-filename=")) {
                value.clone_into(&mut extra_filename);
            }
            // #[gamma::skip(all, reason = "moving backward or not consuming the split option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 2;
        } else if let Some(value) = argument.strip_prefix("-Cextra-filename=") {
            value.clone_into(&mut extra_filename);
            // #[gamma::skip(all, reason = "not consuming the joined option retries the same argument forever and is observed only as a timeout")]
            index += 1;
        } else if separate("--extern") {
            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
            parse_extern(&args[index + 1], &mut externs, &mut opaque_extern);
            // #[gamma::skip(all, reason = "moving backward or not consuming the split option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 2;
        } else if let Some(value) = argument.strip_prefix("--extern=") {
            // #[gamma::skip(call_result.default, reason = "This call returns (); default_after performs the original side effects and substitutes only the identical unit value.", tag = "equivalent")]
            parse_extern(OsStr::new(value), &mut externs, &mut opaque_extern);
            // #[gamma::skip(all, reason = "moving backward or not consuming the joined option makes the parser intrinsically nonterminating and is observed only as a timeout")]
            index += 1;
        } else {
            if !argument.starts_with('-') && argument.ends_with(".rs") && source.is_none() {
                source = utf8(&args[index]);
            }
            // #[gamma::skip(all, reason = "not advancing past an unrecognized argument retries it forever and is observed only as a timeout")]
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
    let Some(value) = value.to_str() else {
        *opaque = true;
        return;
    };
    let Some((_name, path)) = value.split_once('=') else {
        *opaque = true;
        return;
    };

    externs.push(Utf8PathBuf::from(path));
}

fn utf8(value: &OsStr) -> Option<Utf8PathBuf> {
    value.to_str().map(Utf8PathBuf::from)
}

// #[gamma::skip(all, reason = "the mutation affects internal orchestration state with no safely deterministic observation at this layer")]
pub(crate) fn wrapper_path() -> Option<Utf8PathBuf> {
    let path = Utf8PathBuf::from_path_buf(std::env::current_exe().ok()?).ok()?;
    let stem = path.file_stem()?;

    // #[gamma::skip(all, reason = "this literal belongs to a Cargo process protocol or diagnostic boundary that cannot be isolated deterministically without replacing process-global integration state")]
    (stem == "cargo-gamma").then_some(path)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::cell::Cell;
    #[cfg(windows)]
    use std::os::windows::fs::OpenOptionsExt as _;

    use camino::Utf8Path;

    use super::*;

    #[test]
    fn recovered_native_wrapper_returns_the_exact_nonzero_compiler_code_and_forwards_its_command_without_spawning() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                WRAPPER_ENVIRONMENT.with_borrow_mut(|slot| *slot = None);
                COMPILER_OBSERVER.with_borrow_mut(|slot| *slot = None);
            }
        }
        WRAPPER_ENVIRONMENT.with_borrow_mut(|slot| {
            assert!(slot.is_none());
            *slot = Some(core::iter::once((CAPTURE_DIR_VAR.to_owned(), OsString::from("never-written-capture"))).collect());
        });
        let _reset = Reset;
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        COMPILER_OBSERVER.with_borrow_mut(|slot| {
            assert!(slot.is_none());
            *slot = Some(Box::new({
                let calls = std::rc::Rc::clone(&calls);
                move |command| {
                    calls.set(calls.get() + 1);
                    assert_eq!(command.get_program(), OsStr::new("never-launched-rustc"));
                    assert_eq!(command.get_args().collect::<Vec<_>>(), [OsStr::new("--version")]);
                    Ok(Some(7))
                }
            }));
        });
        assert_eq!(
            run_if_requested(["gamma", "never-launched-rustc", "--version"].map(OsString::from)),
            Some(ExitCode::from(7))
        );
        assert_eq!(calls.get(), 1);
    }

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
        assert!(
            parse_invocation(&args(&["--crate-name", "x", "--out-dir", "target"])).is_none(),
            "an invocation without a source path has no compilation identity"
        );
        assert!(parse_invocation(&args(&["--crate-name"])).is_none());
        assert!(parse_invocation(&args(&["--out-dir"])).is_none());
        assert!(parse_invocation(&args(&["--extern"])).is_none());

        let parsed = parse_invocation(&args(&["--crate-name=x", "src/lib.rs", "--out-dir=target", "--extern=opaque"]))
            .expect("identity is otherwise complete");

        assert!(parsed.opaque_extern);
        assert!(parsed.externs.is_empty());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn non_utf8_extern_paths_fail_open_without_recording_a_lossy_path() {
        let mut externs = Vec::new();
        let mut opaque = false;

        parse_extern(&non_utf8_extern(), &mut externs, &mut opaque);

        assert!(opaque);
        assert!(externs.is_empty());
    }

    #[cfg(unix)]
    fn non_utf8_extern() -> OsString {
        use std::os::unix::ffi::OsStringExt as _;

        OsString::from_vec(b"dep=/tmp/libdep-\xff.rlib".to_vec())
    }

    #[cfg(windows)]
    fn non_utf8_extern() -> OsString {
        use std::os::windows::ffi::OsStringExt as _;

        let mut value: Vec<u16> = "dep=C:\\cache\\libdep-".encode_utf16().collect();
        value.push(0xd800);
        value.extend(".rlib".encode_utf16());
        OsString::from_wide(&value)
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

        assert!(reset_capture_directory(&captures));

        assert!(captures.is_dir());
        assert_eq!(fs::read_dir(&captures).expect("capture directory").count(), 0);
    }

    #[test]
    fn a_test_executable_is_not_the_production_wrapper() {
        assert_eq!(wrapper_path(), None);
    }

    #[test]
    fn ordinary_invocations_do_not_enter_wrapper_mode() {
        assert_wrapper_not_requested(None, &["cargo-gamma", "rustc"]);
    }

    #[test]
    fn an_empty_capture_marker_does_not_enter_wrapper_mode() {
        assert_wrapper_not_requested(Some(""), &["cargo-gamma", "rustc"]);
    }

    #[test]
    fn a_capture_marker_without_a_compiler_does_not_enter_wrapper_mode() {
        assert_wrapper_not_requested(Some("captures"), &["cargo-gamma"]);
        assert_wrapper_not_requested(Some("captures"), &[]);
    }

    fn assert_wrapper_not_requested(capture: Option<&str>, invocation: &[&str]) {
        let mut reads = 0;
        let result = run_if_requested_with(
            args(invocation),
            |name| {
                reads += 1;
                assert_eq!(name, CAPTURE_DIR_VAR);
                capture.map(OsString::from)
            },
            || -> Vec<OsString> { panic!("no compiler invocation needs binary dependency discovery") },
            |_| panic!("wrapper mode was not requested"),
            |_, _| panic!("no compiler ran to publish a capture"),
        );
        assert_eq!(result, None);
        assert_eq!(reads, 1);
    }

    #[test]
    fn a_new_build_recovers_from_interrupted_and_truncated_capture_entries() {
        let directory = tempfile::tempdir().expect("capture directory");
        let captures = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 capture path");
        fs::write(captures.join("interrupted.tmp"), br#"{"crate_name":"partial""#).expect("interrupted temporary entry");
        fs::write(captures.join("reused-pid.json"), br#"{"crate_name":"truncated""#).expect("truncated published entry");

        assert!(reset_capture_directory(&captures));

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
    fn a_capture_generation_is_unavailable_when_its_directory_cannot_be_reset() {
        let directory = tempfile::tempdir().expect("test directory");
        let captures = Utf8PathBuf::from_path_buf(directory.path().join("not-a-directory")).expect("UTF-8 capture path");
        fs::write(&captures, b"occupied").expect("capture path file");

        assert!(!reset_capture_directory(&captures));
        assert_eq!(fs::read(&captures).expect("occupied path remains"), b"occupied");
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
    fn wrapper_publication_retries_temporary_and_published_name_collisions() {
        let directory = tempfile::tempdir().expect("capture directory");
        let captures = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 capture path");
        fs::write(captures.join("occupied-temporary.tmp"), b"temporary").expect("temporary collision");
        fs::write(captures.join("occupied-published.json"), b"published").expect("published collision");
        let mut stems = ["occupied-temporary", "occupied-published", "fresh"].into_iter();

        publish_capture_with(&captures, b"capture", || {
            stems.next().expect("the collision fixture provides enough names").to_owned()
        })
        .expect("collisions are retried");

        assert_eq!(
            fs::read(captures.join("occupied-published.json")).expect("old capture"),
            b"published"
        );
        assert_eq!(fs::read(captures.join("fresh.json")).expect("new capture"), b"capture");
        assert!(!captures.join("occupied-published.tmp").exists());
    }

    #[test]
    fn publication_returns_noncollision_open_errors_without_retrying_or_creating_a_capture() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let captures = Utf8Path::from_path(directory.path()).unwrap();
        let mut calls = 0;
        let failure = publish_capture_with(captures, b"capture", || {
            calls += 1;
            assert_eq!(calls, 1, "only name collisions may request another stem");
            "invalid\0stem".into()
        })
        .expect_err("a NUL-containing capture name cannot be opened");
        assert_eq!(failure.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(calls, 1);
        assert_eq!(fs::read_dir(captures).unwrap().count(), 0);
        let blocker = captures.join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        publish_capture_with(&blocker.join("captures"), b"capture", || {
            panic!("directory creation failed before choosing a name")
        })
        .expect_err("a regular file cannot contain a capture directory");
        assert_eq!(fs::read_to_string(blocker).unwrap(), "not a directory");
    }

    #[test]
    fn capture_publication_propagates_late_write_flush_and_noncollision_link_failures_without_publishing_partial_bytes() {
        for stage in 0..3 {
            let directory = tempfile::tempdir_in(".").unwrap();
            let captures = Utf8Path::from_path(directory.path()).unwrap();
            let blocker = captures.join("blocker");
            fs::write(&blocker, "not a directory").unwrap();
            let links = std::cell::Cell::new(0);
            let failure = publish_capture_with_io(
                captures,
                b"complete capture bytes",
                || "bounded".to_owned(),
                |file, bytes| {
                    if stage == 0 {
                        file.write_all(&bytes[..3])?;
                        Err(std::io::Error::other("late capture write refusal"))
                    } else {
                        file.write_all(bytes)
                    }
                },
                |file| {
                    assert_ne!(stage, 0, "failed writes cannot proceed to flushing");
                    assert_eq!(file.metadata()?.len(), 22);
                    if stage == 1 {
                        Err(std::io::Error::other("late capture flush refusal"))
                    } else {
                        file.sync_data()
                    }
                },
                |temporary, published| {
                    links.set(links.get() + 1);
                    assert_eq!(links.get(), 1, "only AlreadyExists can retry publication");
                    assert_eq!(stage, 2, "failed writes or flushes cannot proceed to publication");
                    assert_eq!(fs::read(temporary)?, b"complete capture bytes");
                    assert_eq!(published, captures.join("bounded.json").as_std_path());
                    fs::hard_link(temporary, blocker.join("child"))
                },
            )
            .expect_err("each exact late publication failure must escape without unwrap or successful partial publication");
            match stage {
                0 => assert_eq!(failure.to_string(), "late capture write refusal"),
                1 => assert_eq!(failure.to_string(), "late capture flush refusal"),
                _ => assert_ne!(failure.kind(), std::io::ErrorKind::AlreadyExists),
            }
            assert_eq!(
                fs::read(captures.join("bounded.tmp")).unwrap(),
                if stage == 0 {
                    b"com".as_slice()
                } else {
                    b"complete capture bytes".as_slice()
                }
            );
            assert!(!captures.join("bounded.json").exists());
            assert_eq!(fs::read_to_string(&blocker).unwrap(), "not a directory");
        }
    }

    #[test]
    #[cfg(windows)]
    fn resetting_a_capture_generation_rejects_native_delete_denial_even_when_its_directory_still_exists() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let captures = Utf8Path::from_path(directory.path()).unwrap().join("captures");
        fs::create_dir(&captures).unwrap();
        let prior = captures.join("prior.json");
        fs::write(&prior, "prior capture bytes").unwrap();
        let locked = fs::File::options().read(true).share_mode(1).open(&prior).unwrap();
        assert!(!reset_capture_directory(&captures));
        assert!(
            captures.is_dir(),
            "directory existence cannot prove that its old generation was cleared"
        );
        assert_eq!(fs::read_to_string(&prior).unwrap(), "prior capture bytes");
        drop(locked);
        assert!(reset_capture_directory(&captures));
        assert_eq!(fs::read_dir(&captures).unwrap().count(), 0);
    }

    #[test]
    fn wrapper_boundary_keeps_the_compiler_out_of_direct_arguments_and_preserves_both_sources_of_opaque_dependencies() {
        for original in [None, Some(""), Some("original-wrapper")] {
            for (preopaque, binary) in [(false, false), (false, true), (true, false), (true, true)] {
                let mut invocation = args(&[
                    "cargo-gamma",
                    "decoy.rs",
                    "--crate-name=wrapped",
                    "--crate-type=lib",
                    "--out-dir=target",
                    "src/real.rs",
                ]);
                if preopaque {
                    invocation.push("--extern=opaque".into());
                }
                let expected_args = invocation[if original == Some("original-wrapper") { 1 } else { 2 }..].to_vec();
                let mut reads = Vec::new();
                let ran = Cell::new(false);
                let mut published = false;
                let result = run_if_requested_with(
                    invocation,
                    |name| {
                        reads.push(name.to_owned());
                        match name {
                            CAPTURE_DIR_VAR => Some("captures".into()),
                            ORIGINAL_WRAPPER_VAR => original.map(OsString::from),
                            _ => panic!("unexpected environment read: {name}"),
                        }
                    },
                    || {
                        assert!(ran.get(), "dependency discovery follows compiler success");
                        let mut names = args(&["NONPREFIX_CARGO_BIN_EXE_", "CARGO_BIN_EXE"]);
                        if binary {
                            names.push("CARGO_BIN_EXE_boundary".into());
                        }
                        names
                    },
                    |command| {
                        assert_eq!(
                            command.get_program(),
                            OsStr::new(original.filter(|value| !value.is_empty()).unwrap_or("decoy.rs"))
                        );
                        assert_eq!(command.get_args().collect::<Vec<_>>(), expected_args);
                        assert_eq!(command.get_current_dir(), None);
                        assert_eq!(command.get_envs().count(), 0);
                        ran.set(true);
                        Ok(Some(0))
                    },
                    |directory, bytes| {
                        assert_eq!(directory, Utf8Path::new("captures"));
                        assert_eq!(
                            serde_json::from_slice::<RustcInvocation>(bytes).unwrap(),
                            RustcInvocation {
                                crate_name: "wrapped".into(),
                                crate_types: vec!["lib".into()],
                                test: false,
                                source: "src/real.rs".into(),
                                out_dir: "target".into(),
                                extra_filename: String::new(),
                                externs: Vec::new(),
                                opaque_extern: preopaque || binary,
                            }
                        );
                        published = true;
                        Ok(())
                    },
                );
                assert_eq!(result, Some(ExitCode::SUCCESS));
                assert_eq!(reads, [CAPTURE_DIR_VAR, ORIGINAL_WRAPPER_VAR]);
                assert!(ran.get());
                assert!(published);
            }
        }
    }

    #[test]
    fn capture_storage_failures_do_not_change_the_compiler_result() {
        let mut published = false;
        let result = run_if_requested_with(
            args(&["cargo-gamma", "rustc", "--crate-name=wrapped", "src/lib.rs", "--out-dir=target"]),
            wrapper_environment,
            Vec::<OsString>::new,
            |_| Ok(Some(0)),
            |directory, bytes| {
                assert_eq!(directory, Utf8Path::new("captures"));
                assert_eq!(serde_json::from_slice::<RustcInvocation>(bytes).unwrap().crate_name, "wrapped");
                published = true;
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "scripted capture refusal",
                ))
            },
        );
        assert_eq!(result, Some(ExitCode::SUCCESS));
        assert!(published);
    }

    fn wrapper_environment(name: &str) -> Option<OsString> {
        match name {
            CAPTURE_DIR_VAR => Some("captures".into()),
            ORIGINAL_WRAPPER_VAR => None,
            _ => panic!("unexpected environment read: {name}"),
        }
    }

    #[test]
    fn compiler_failures_preserve_exit_codes_without_discovering_or_publishing_captures() {
        for (code, expected) in [
            (Some(7), ExitCode::from(7)),
            (Some(255), ExitCode::from(255)),
            (Some(256), ExitCode::FAILURE),
            (Some(-1), ExitCode::FAILURE),
            (None, ExitCode::FAILURE),
        ] {
            assert_compiler_failure(Ok(code), expected);
        }
        assert_compiler_failure(
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "scripted spawn failure")),
            ExitCode::FAILURE,
        );
    }

    fn assert_compiler_failure(status: std::io::Result<Option<i32>>, expected: ExitCode) {
        let mut ran = false;
        let result = run_if_requested_with(
            args(&["cargo-gamma", "rustc", "--crate-name=wrapped", "src/lib.rs", "--out-dir=target"]),
            wrapper_environment,
            || -> Vec<OsString> { panic!("a failed compiler has no dependency capture") },
            |command| {
                assert_eq!(command.get_program(), OsStr::new("rustc"));
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    args(&["--crate-name=wrapped", "src/lib.rs", "--out-dir=target"])
                );
                ran = true;
                status
            },
            |_, _| panic!("a failed compiler must not publish a capture"),
        );
        assert_eq!(result, Some(expected));
        assert!(ran);
    }

    #[test]
    fn successful_noncompilation_invocations_do_not_publish_captures() {
        let result = run_if_requested_with(
            args(&["cargo-gamma", "rustc", "--version"]),
            wrapper_environment,
            || -> Vec<OsString> { panic!("an invocation without compilation identity has no dependencies") },
            |command| {
                assert_eq!(command.get_program(), OsStr::new("rustc"));
                assert_eq!(command.get_args().collect::<Vec<_>>(), args(&["--version"]));
                Ok(Some(0))
            },
            |_, _| panic!("an invocation without compilation identity must not publish a capture"),
        );
        assert_eq!(result, Some(ExitCode::SUCCESS));
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
