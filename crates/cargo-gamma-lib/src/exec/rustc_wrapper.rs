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
pub(crate) const CAP_LINTS_VAR: &str = "CARGO_GAMMA_CAP_LINTS";

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
    super::rustc_replay::compiler_status(command)
}

fn run_if_requested_with<I: IntoIterator<Item = OsString>>(
    args: impl IntoIterator<Item = OsString>,
    mut environment: impl FnMut(&str) -> Option<OsString>,
    binary_environment: impl FnOnce() -> I,
    run: impl FnOnce(&mut Command) -> std::io::Result<Option<i32>>,
    publish: impl FnOnce(&camino::Utf8Path, &[u8]) -> std::io::Result<()>,
) -> Option<ExitCode> {
    let directory = environment(CAPTURE_DIR_VAR).filter(|directory| !directory.is_empty());
    if directory.is_none() && environment(CAP_LINTS_VAR).is_none() {
        return None;
    }
    let mut args = args.into_iter();
    let _self = args.next();
    let forwarded: Vec<OsString> = args.collect();
    let compiler = forwarded.first()?;

    let mut command = if let Some(wrapper) = environment(ORIGINAL_WRAPPER_VAR).filter(|path| !path.is_empty()) {
        let mut command = Command::new(wrapper);
        let _ = command.args(&forwarded);
        // Consume chaining without removing the marker that disables replay for user wrappers.
        let _ = command.env(ORIGINAL_WRAPPER_VAR, "");
        command
    } else {
        let mut command = Command::new(compiler);
        let _ = command.args(&forwarded[1..]);
        command
    };

    // Cargo has already resolved target, ancestor, and Cargo-home rustflags here. Appending last
    // makes the cap independent of which source won, without replacing any user flags.
    if parse_invocation(&forwarded[1..]).is_some() {
        let _command = command.arg("--cap-lints=allow");
    }

    let Ok(code) = run(&mut command) else {
        return Some(ExitCode::FAILURE);
    };

    if code == Some(0)
        && let Some(directory) = directory
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
    reset_capture_directory_with(
        directory,
        |path| fs::remove_dir_all(path.as_std_path()),
        |path| fs::create_dir_all(path.as_std_path()),
    )
}

fn reset_capture_directory_with(
    directory: &camino::Utf8Path,
    remove: impl FnOnce(&camino::Utf8Path) -> std::io::Result<()>,
    create: impl FnOnce(&camino::Utf8Path) -> std::io::Result<()>,
) -> bool {
    match remove(directory) {
        Ok(()) => {}
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {}
        Err(_cause) => return false,
    }

    create(directory).is_ok()
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

/// Replaces one cache entry only after the complete replacement has been flushed.
pub(super) fn publish_atomic(path: &camino::Utf8Path, bytes: &[u8]) -> std::io::Result<()> {
    publish_atomic_with(path, bytes, |from, to| fs::rename(from, to))
}

fn publish_atomic_with(
    path: &camino::Utf8Path,
    bytes: &[u8],
    replace: impl FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    publish_atomic_with_stem(path, bytes, replace, next_capture_stem)
}

fn publish_atomic_with_stem(
    path: &camino::Utf8Path,
    bytes: &[u8],
    replace: impl FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
    mut next_stem: impl FnMut() -> String,
) -> std::io::Result<()> {
    let directory =
        // #[gamma::skip(try.propagate_to_unwrap, literal.str_to_empty, literal.str_to_xyzzy, tag = "equivalent", reason = "Every production destination joins a record, output-index or runtime filename to its directory, so the parent exists and this error text is unreachable.")]
        path.parent().ok_or_else(|| std::io::Error::other("cache entry has no parent"))?;
    fs::create_dir_all(directory)?;
    let (temporary, mut file) = loop {
        let temporary = directory.join(format!("{}.tmp", next_stem()));
        match OpenOptions::new().create_new(true).write(true).open(&temporary) {
            Ok(file) => break (temporary, file),
            Err(cause) if cause.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(cause) => return Err(cause),
        }
    };
    let result = file.write_all(bytes).and_then(|()| file.sync_data());
    drop(file);
    let result = result.and_then(|()| replace(temporary.as_std_path(), path.as_std_path()));
    let _removed = fs::remove_file(temporary);
    result
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
    let mut arguments = args.iter();

    while let Some(value) = arguments.next() {
        let argument = value.to_string_lossy();
        let has_value = !arguments.as_slice().is_empty();
        let separate = |name: &str|
            // #[gamma::skip(logical.and_remove_right, tag = "supported-input", reason = "Successful rustc invocations supply values for these separate-value flags. Dangling flags are rejected by rustc and cannot produce a compiler capture.")]
            argument == name && has_value;

        if separate("--crate-name") {
            crate_name = Some(
                // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "separate verifies a remaining value and the iterator is unchanged before this next call.")]
                arguments.next()?.to_string_lossy().into_owned(),
            );
        } else if let Some(value) = argument.strip_prefix("--crate-name=") {
            crate_name = Some(value.to_owned());
        } else if separate("--crate-type") {
            crate_types.extend(
                // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "separate verifies a remaining value and the iterator is unchanged before this next call.")]
                arguments.next()?.to_string_lossy().split(',').map(str::to_owned),
            );
        } else if let Some(value) = argument.strip_prefix("--crate-type=") {
            crate_types.extend(value.split(',').map(str::to_owned));
        } else if argument == "--test" {
            test = true;
        } else if separate("--out-dir") {
            out_dir = utf8(
                // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "separate verifies a remaining value and the iterator is unchanged before this next call.")]
                arguments.next()?,
            );
        } else if let Some(value) = argument.strip_prefix("--out-dir=") {
            out_dir = Some(Utf8PathBuf::from(value));
        } else if separate("-C") {
            if let Some(value) =
                // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "separate verifies a remaining value and the iterator is unchanged before this next call.")]
                arguments.next()?.to_str().and_then(|value| value.strip_prefix("extra-filename="))
            {
                value.clone_into(&mut extra_filename);
            }
        } else if let Some(value) = argument.strip_prefix("-Cextra-filename=") {
            value.clone_into(&mut extra_filename);
        } else if separate("--extern") {
            parse_extern(
                // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "separate verifies a remaining value and the iterator is unchanged before this next call.")]
                arguments.next()?,
                &mut externs,
                &mut opaque_extern,
            );
        } else if let Some(value) = argument.strip_prefix("--extern=") {
            parse_extern(OsStr::new(value), &mut externs, &mut opaque_extern);
        } else if !argument.starts_with('-') && argument.ends_with(".rs") && source.is_none() {
            source = utf8(value);
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

pub(crate) fn wrapper_path() -> Option<Utf8PathBuf> {
    wrapper_path_with(std::env::current_exe)
}

fn wrapper_path_with(current_exe: impl FnOnce() -> std::io::Result<std::path::PathBuf>) -> Option<Utf8PathBuf> {
    let path = Utf8PathBuf::from_path_buf(current_exe().ok()?).ok()?;
    let stem =
        // #[gamma::skip(try.propagate_to_unwrap, tag = "native-boundary", reason = "A successful native current_exe lookup identifies an executable filename, not a directory root, so its file stem exists.")]
        path.file_stem()?;

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
    fn the_final_lint_cap_wins_over_cargo_resolved_target_flags_even_without_captures() {
        let result = run_if_requested_with(
            args(&[
                "gamma",
                "rustc",
                "--crate-name=host",
                "src/lib.rs",
                "--out-dir=target",
                "--cfg=parent_target",
                "--cap-lints=deny",
            ]),
            |name| match name {
                CAPTURE_DIR_VAR | ORIGINAL_WRAPPER_VAR => None,
                CAP_LINTS_VAR => Some("1".into()),
                _ => panic!("unexpected lookup"),
            },
            || -> Vec<OsString> { panic!("lint-only wrapping must not capture dependencies") },
            |command| {
                let forwarded = command.get_args().collect::<Vec<_>>();
                assert!(forwarded.contains(&OsStr::new("--cfg=parent_target")), "user flags are preserved");
                assert_eq!(forwarded.last().copied(), Some(OsStr::new("--cap-lints=allow")));
                Ok(Some(0))
            },
            |_, _| panic!("lint-only wrapping must not publish into an unavailable capture generation"),
        );
        assert_eq!(result, Some(ExitCode::SUCCESS));
    }

    #[test]
    fn configured_outer_and_workspace_wrappers_keep_cargos_nesting_and_the_final_cap() {
        let invocation = [
            "gamma",
            "workspace-wrapper",
            "rustc",
            "--crate-name=subject",
            "src/lib.rs",
            "--out-dir=target",
            "--cfg=kept",
            "--cap-lints=deny",
        ];
        let result = run_if_requested_with(
            args(&invocation),
            |name| match name {
                CAPTURE_DIR_VAR => None,
                CAP_LINTS_VAR => Some("1".into()),
                ORIGINAL_WRAPPER_VAR => Some("resolved-outer-wrapper".into()),
                _ => panic!("unexpected lookup"),
            },
            || -> Vec<OsString> { panic!("lint-only mode has no captures") },
            |command| {
                assert_eq!(command.get_program(), OsStr::new("resolved-outer-wrapper"));
                assert!(
                    command
                        .get_envs()
                        .any(|(name, value)| name == ORIGINAL_WRAPPER_VAR && value == Some(OsStr::new(""))),
                    "the selected wrapper must not inherit a request to invoke itself again"
                );
                let mut expected = args(&invocation[1..]);
                expected.push("--cap-lints=allow".into());
                assert_eq!(command.get_args().map(OsString::from).collect::<Vec<_>>(), expected);
                Ok(Some(0))
            },
            |_, _| panic!("lint-only mode has no captures"),
        );
        assert_eq!(result, Some(ExitCode::SUCCESS));
    }

    #[test]
    fn a_bare_self_wrapper_runs_the_compiler_once_instead_of_recursing() {
        let invocation = ["gamma", "rustc", "--crate-name=subject", "src/lib.rs", "--out-dir=target"];
        let mut compiler_calls = 0;
        let result = run_if_requested_with(
            args(&invocation),
            |name| match name {
                CAP_LINTS_VAR => Some("1".into()),
                ORIGINAL_WRAPPER_VAR => Some("gamma".into()),
                _ => None,
            },
            Vec::<OsString>::new,
            |outer| {
                assert_eq!(outer.get_program(), OsStr::new("gamma"));
                let inner = run_if_requested_with(
                    std::iter::once(outer.get_program().to_owned()).chain(outer.get_args().map(OsStr::to_owned)),
                    |name| {
                        outer.get_envs().find(|(key, _)| *key == name).map_or_else(
                            || match name {
                                CAP_LINTS_VAR => Some("1".into()),
                                ORIGINAL_WRAPPER_VAR => Some("gamma".into()),
                                _ => None,
                            },
                            |(_, value)| value.map(OsStr::to_owned),
                        )
                    },
                    Vec::<OsString>::new,
                    |compiler| {
                        compiler_calls += 1;
                        assert_eq!(compiler.get_program(), OsStr::new("rustc"));
                        assert_eq!(compiler.get_args().last(), Some(OsStr::new("--cap-lints=allow")));
                        Ok(Some(0))
                    },
                    |_, _| panic!("lint-only mode must not publish captures"),
                );
                assert_eq!(inner, Some(ExitCode::SUCCESS));
                Ok(Some(0))
            },
            |_, _| panic!("lint-only mode must not publish captures"),
        );
        assert_eq!(result, Some(ExitCode::SUCCESS));
        assert_eq!(compiler_calls, 1);
    }

    #[test]
    fn capture_reset_declines_remove_and_create_failures_but_accepts_absent_generations() {
        let directory = Utf8Path::new("captures");
        assert!(!reset_capture_directory_with(
            directory,
            |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            |_| panic!("a failed cleanup cannot create a usable generation")
        ));
        for absent in [false, true] {
            for created in [false, true] {
                assert_eq!(
                    reset_capture_directory_with(
                        directory,
                        |_| if absent {
                            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
                        } else {
                            Ok(())
                        },
                        |path| {
                            assert_eq!(path, directory);
                            if created {
                                Ok(())
                            } else {
                                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                            }
                        }
                    ),
                    created
                );
            }
        }
    }

    #[test]
    fn atomic_replacement_keeps_the_old_record_until_complete_publication_and_cleans_failed_staging() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = Utf8Path::from_path(directory.path()).unwrap().join("record.json");
        fs::write(&path, b"old complete record").unwrap();
        let failure = publish_atomic_with(&path, b"new complete record", |temporary, published| {
            assert_eq!(fs::read(temporary).unwrap(), b"new complete record");
            assert_eq!(fs::read(published).unwrap(), b"old complete record");
            Err(std::io::Error::other("publication refused"))
        })
        .unwrap_err();
        assert_eq!(failure.to_string(), "publication refused");
        assert_eq!(fs::read(&path).unwrap(), b"old complete record");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        publish_atomic(&path, b"new complete record").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new complete record");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

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
    fn nov21_atomic_cache_publication_retries_an_existing_temporary_but_not_a_disappeared_directory() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
        let destination = root.join("record.json");
        fs::write(root.join("occupied.tmp"), b"previous writer").unwrap();
        let mut stems = ["occupied".to_owned(), "fresh".to_owned()].into_iter();
        publish_atomic_with_stem(
            &destination,
            b"complete replacement",
            |from, to| {
                assert_eq!(fs::read(from).unwrap(), b"complete replacement");
                fs::rename(from, to)
            },
            || stems.next().expect("one collision must consume exactly two proposed names"),
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"complete replacement");
        assert_eq!(fs::read(root.join("occupied.tmp")).unwrap(), b"previous writer");
        assert!(!root.join("fresh.tmp").exists());

        let vanished = root.join("vanished");
        let mut attempts = 0;
        let failure = publish_atomic_with_stem(
            &vanished.join("record.json"),
            b"must not publish",
            |_, _| panic!("failed exclusive creation must not publish"),
            || {
                attempts += 1;
                assert_eq!(attempts, 1, "a noncollision error must not be retried");
                fs::remove_dir(&vanished).unwrap();
                "fresh".into()
            },
        )
        .unwrap_err();
        assert_eq!(failure.kind(), std::io::ErrorKind::NotFound);
        assert!(!vanished.exists());
    }

    #[test]
    fn nov21_atomic_cache_publication_propagates_an_uncreatable_parent() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
        let blocker = root.join("not-a-directory");
        fs::write(&blocker, b"preserved").unwrap();
        assert!(
            publish_atomic_with(&blocker.join("record.json"), b"replacement", |_, _| panic!(
                "an uncreatable parent must not reach replacement"
            ),)
            .is_err()
        );
        assert_eq!(fs::read(blocker).unwrap(), b"preserved");
    }

    #[test]
    fn nov21_split_out_directory_and_codegen_values_are_consumed_before_the_source() {
        let parsed = parse_invocation(&args(&[
            "--crate-name=subject",
            "--out-dir",
            "output.rs",
            "-C",
            "extra-filename=-suffix.rs",
            "--crate-type",
            "lib,rlib",
            "--extern",
            "dep=target/libdep.rlib",
            "--cfg=kept",
            "src/subject.rs",
        ]))
        .unwrap();
        assert_eq!(parsed.source, Utf8Path::new("src/subject.rs"));
        assert_eq!(parsed.out_dir, Utf8Path::new("output.rs"));
        assert_eq!(parsed.extra_filename, "-suffix.rs");
        assert_eq!(parsed.crate_types, ["lib", "rlib"]);
        assert_eq!(parsed.externs, [Utf8PathBuf::from("target/libdep.rlib")]);
    }

    #[test]
    fn nov21_each_split_or_joined_option_leaves_the_following_compiler_option_visible() {
        for option in [
            vec!["--crate-name", "subject"],
            vec!["--crate-name=subject"],
            vec!["--crate-type", "lib,rlib"],
            vec!["--crate-type=lib,rlib"],
            vec!["--out-dir", "output"],
            vec!["--out-dir=output"],
            vec!["-C", "extra-filename=-unit.rs"],
            vec!["-Cextra-filename=-unit"],
            vec!["--extern", "dep=target/libdep.rlib"],
            vec!["--extern=dep=target/libdep.rlib"],
            vec!["--cfg=enabled"],
        ] {
            let mut invocation = vec!["--cfg=leading"; 3];
            invocation.extend(option);
            invocation.extend(["--out-dir=output", "--crate-name=subject", "src/subject.rs"]);
            let parsed = parse_invocation(&args(&invocation)).unwrap();
            assert_eq!(parsed.source, Utf8Path::new("src/subject.rs"));
            assert_eq!(parsed.crate_name, "subject");
            assert_eq!(parsed.out_dir, Utf8Path::new("output"));
        }
    }

    #[test]
    fn nov21_split_and_joined_scans_at_index_zero_cannot_move_backwards() {
        for option in [
            vec!["--crate-type", "lib,rlib"],
            vec!["--crate-type=lib,rlib"],
            vec!["--out-dir", "output"],
            vec!["--out-dir=output"],
            vec!["-C", "extra-filename=-unit"],
            vec!["-Cextra-filename=-unit"],
            vec!["--extern", "dep=target/libdep.rlib"],
            vec!["--extern=dep=target/libdep.rlib"],
        ] {
            let mut invocation = option;
            invocation.extend(["--crate-name=subject", "--out-dir=output", "src/subject.rs"]);
            let parsed = parse_invocation(&args(&invocation)).unwrap();
            assert_eq!(parsed.crate_name, "subject");
            assert_eq!(parsed.out_dir, Utf8Path::new("output"));
            assert_eq!(parsed.source, Utf8Path::new("src/subject.rs"));
        }
    }

    #[test]
    fn nov21_parser_exhausts_long_codegen_lists_without_rescanning_value_paths() {
        let mut invocation = args(&["--crate-name=subject", "--crate-type=lib", "--out-dir=output"]);
        for _ in 0..64 {
            invocation.extend(args(&["-C", "metadata=not-the-source.rs"]));
        }
        invocation.extend(args(&["--cfg=enabled", "src/subject.rs"]));
        let parsed = parse_invocation(&invocation).unwrap();
        assert_eq!(parsed.source, Utf8Path::new("src/subject.rs"));
        assert_eq!(parsed.crate_types, ["lib"]);
        assert_eq!(parsed.extra_filename, "");
    }

    #[test]
    fn nov21_a_custom_compiler_path_is_not_a_source_when_deciding_lint_capping() {
        let invocation = [
            "gamma",
            "custom-compiler.rs",
            "--crate-name=subject",
            "--out-dir=output",
            "src/subject.custom",
        ];
        let result = run_if_requested_with(
            args(&invocation),
            |name| (name == CAP_LINTS_VAR).then(|| "1".into()),
            || -> Vec<OsString> { panic!("lint-only mode must not enumerate capture inputs") },
            |command| {
                assert_eq!(command.get_program(), OsStr::new("custom-compiler.rs"));
                assert_eq!(command.get_args().map(OsString::from).collect::<Vec<_>>(), args(&invocation[2..]));
                Ok(Some(0))
            },
            |_, _| panic!("a noncapturable custom source has no publication"),
        );
        assert_eq!(result, Some(ExitCode::SUCCESS));
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
        assert_eq!(parsed.externs.len(), 0);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn non_utf8_extern_paths_fail_open_without_recording_a_lossy_path() {
        let mut externs = Vec::new();
        let mut opaque = false;

        parse_extern(&non_utf8_extern(), &mut externs, &mut opaque);

        assert!(opaque);
        assert_eq!(externs.len(), 0);
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
    fn nov21_unavailable_or_non_unicode_executable_paths_decline_wrapper_interposition() {
        assert_eq!(
            wrapper_path_with(|| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))),
            None,
        );
        #[cfg(windows)]
        let name = {
            use std::os::windows::ffi::OsStringExt as _;
            OsString::from_wide(&[0xd800])
        };
        #[cfg(unix)]
        let name = {
            use std::os::unix::ffi::OsStringExt as _;
            OsString::from_vec(vec![0xff])
        };
        let path = std::env::current_dir().unwrap().join(name);
        assert_eq!(wrapper_path_with(|| Ok(path)), None);
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
                assert!(matches!(name, CAPTURE_DIR_VAR | CAP_LINTS_VAR));
                (name == CAPTURE_DIR_VAR).then(|| capture.map(OsString::from)).flatten()
            },
            || -> Vec<OsString> { panic!("no compiler invocation needs binary dependency discovery") },
            |_| panic!("wrapper mode was not requested"),
            |_, _| panic!("no compiler ran to publish a capture"),
        );
        assert_eq!(result, None);
        assert_eq!(reads, if capture.is_some_and(|value| !value.is_empty()) { 1 } else { 2 });
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
                let mut expected_args = invocation[if original == Some("original-wrapper") { 1 } else { 2 }..].to_vec();
                expected_args.push("--cap-lints=allow".into());
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
                        assert_eq!(command.get_envs().count(), usize::from(original == Some("original-wrapper")));
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
                    args(&["--crate-name=wrapped", "src/lib.rs", "--out-dir=target", "--cap-lints=allow"])
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
