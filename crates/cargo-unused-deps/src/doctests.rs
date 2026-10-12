// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Doctest evidence, gathered by standing in for the compiler rustdoc uses.
//!
//! `--all-targets` never builds doctests, so no ordinary invocation reports the
//! dependencies they use. rustdoc does run the lint while compiling them, but
//! discards the compiler's stderr when compilation succeeds. It also allows the
//! compiler to be replaced, so this tool replaces it with itself: the shim runs
//! the real rustc with the lint enabled and keeps a copy of the diagnostics
//! rustdoc would have thrown away.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitCode, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use ohno::{AppError, IntoAppError};

type Result<T> = std::result::Result<T, AppError>;

/// Environment variable naming the file the shim appends its findings to.
///
/// Its presence is also what puts this binary into shim mode, since rustdoc
/// invokes a test builder as a bare rustc and passes no flag of our own.
pub const CAPTURE_VAR: &str = "CARGO_UNUSED_DEPS_CAPTURE";

/// Marks an invocation of this binary as Cargo's rustdoc executable.
pub const RUSTDOC_WRAPPER_VAR: &str = "CARGO_UNUSED_DEPS_RUSTDOC_WRAPPER";

/// Preserves a caller-selected rustdoc executable behind this tool's wrapper.
const INNER_RUSTDOC_VAR: &str = "CARGO_UNUSED_DEPS_INNER_RUSTDOC";

/// Distinguishes several shim invocations in one process.
static CAPTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// What one package's doctests did and did not use.
#[derive(Debug, Default, Clone)]
pub struct PackageDoctests {
    /// How many doctests rustdoc compiled.
    compiled: usize,

    /// How many doctests reported each dependency unused.
    reports: BTreeMap<String, usize>,
}

/// Doctest evidence for a whole run, keyed by package manifest.
#[derive(Debug, Default)]
pub struct DoctestEvidence {
    /// Per-package findings.
    packages: BTreeMap<std::path::PathBuf, PackageDoctests>,
}

impl DoctestEvidence {
    /// Record one package's findings.
    pub fn insert(&mut self, package: std::path::PathBuf, doctests: PackageDoctests) {
        self.packages.insert(package, doctests);
    }

    /// Whether a doctest of `package` used `name`.
    ///
    /// Every doctest is a separate compilation with the dependency in scope, so
    /// the counting is the same as for targets: fewer reports than doctests
    /// means some doctest used it. Treating a single report as proof of disuse
    /// would convict a dependency that one example needs and the other 174 do
    /// not mention.
    pub fn used(&self, package: &Path, name: &str) -> bool {
        self.packages.get(package).is_some_and(|doctests| {
            let reports = doctests.reports.get(name).copied().unwrap_or_default();

            reports < doctests.compiled
        })
    }
}

/// Run as the compiler rustdoc invokes for doctests.
///
/// Execs the real rustc with the lint turned on, records what it reported, and
/// forwards both its diagnostics and its exit status so a failing doctest still
/// fails in the ordinary way.
///
/// # Errors
///
/// Returns an error when rustc cannot be launched or the capture file cannot be
/// written.
pub fn shim(args: &[OsString], capture: &Path) -> Result<ExitCode> {
    let rustc = selected_rustc(|key| std::env::var_os(key));
    let args = stable_diagnostic_args(args);
    let output = Command::new(&rustc)
        .args(&args)
        .arg("--force-warn")
        .arg("unused_crate_dependencies")
        // #[gamma::skip(collection.reverse_array, reason = "these two independent rustc diagnostic flags set different options; their order has no effect")]
        .args(["--error-format=human", "--color=never"])
        // rustdoc feeds the doctest source on stdin; without this the shim
        // would hand rustc an empty program and every doctest would fail.
        .stdin(Stdio::inherit())
        .output();

    capture_compiler_output(capture, output, &mut std::io::stderr())
}

fn capture_compiler_output(capture: &Path, output: std::io::Result<Output>, diagnostics: &mut dyn std::io::Write) -> Result<ExitCode> {
    let output = output.into_app_err("failed to run rustc from the doctest shim")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    record(capture, &stderr)?;

    // rustdoc reads the compiler's stderr to report a failing doctest, so it is
    // passed through rather than swallowed. The format was normalized to
    // human/no-color above because the capture parser and rustdoc both consume
    // those diagnostics.
    diagnostics
        .write_all(&output.stderr)
        .into_app_err("failed to forward rustc diagnostics")?;

    Ok(if output.status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Preserve compiler semantics while normalizing diagnostics for the parser.
fn stable_diagnostic_args(args: &[OsString]) -> Vec<OsString> {
    let mut kept = Vec::with_capacity(args.len());
    let mut skip_value = false;
    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        let text = arg.to_string_lossy();
        if matches!(text.as_ref(), "--error-format" | "--color" | "--json") {
            skip_value = true;
        } else if !text.starts_with("--error-format=") && !text.starts_with("--color=") && !text.starts_with("--json=") {
            kept.push(arg.clone());
        }
    }
    kept
}

/// Run as Cargo's rustdoc executable and install the doctest compiler shim.
pub fn rustdoc_wrapper(args: &[OsString]) -> Result<ExitCode> {
    rustdoc_wrapper_with(args, std::env::var_os(INNER_RUSTDOC_VAR), std::env::current_exe(), |mut command| {
        command.status()
    })
}

fn rustdoc_wrapper_with(
    args: &[OsString],
    selected: Option<OsString>,
    executable: std::io::Result<std::path::PathBuf>,
    run: impl FnOnce(Command) -> std::io::Result<ExitStatus>,
) -> Result<ExitCode> {
    let rustdoc = tool_or_default(selected, "rustdoc");
    let shim = executable.into_app_err("failed to locate this executable to use as the doctest shim")?;
    let mut command = Command::new(rustdoc);
    command
        .args(args)
        .args(["-Z", "unstable-options", "--no-run", "--test-builder"])
        .arg(shim)
        .env_remove(RUSTDOC_WRAPPER_VAR)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = run(command).into_app_err("failed to run rustdoc from the doctest wrapper")?;

    Ok(if status.success() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// Write the findings from one doctest to its own capture record.
///
/// The format is deliberately trivial -- a bare line to record that a doctest
/// compiled, and a `\tname` line per unused dependency -- because both writer
/// and reader live in this crate.
fn record(capture: &Path, stderr: &str) -> Result<()> {
    use std::io::Write as _;

    let mut lines = String::from("compiled\n");
    for name in stderr.lines().filter_map(unused_name) {
        lines.push('\t');
        lines.push_str(&name);
        lines.push('\n');
    }

    let sequence = next_capture_sequence(&CAPTURE_SEQUENCE);
    let record = capture.join(format!("{}-{sequence}.tsv", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&record)
        .into_app_err(format!("failed to create the doctest capture record {}", record.display()))?;

    file.write_all(lines.as_bytes())
        .into_app_err(format!("failed to write the doctest capture record {}", record.display()))
}

fn next_capture_sequence(sequence: &AtomicUsize) -> usize {
    sequence.fetch_add(1, Ordering::Relaxed)
}

/// The dependency named by one output line from rustc, if it is the lint.
fn unused_name(line: &str) -> Option<String> {
    let text = line.trim();
    if !text.starts_with("warning: extern crate ") && !text.starts_with("error: extern crate ") {
        return None;
    }

    let (_, after) = text.split_once('`')?;
    let (name, _) = after.split_once('`')?;

    Some(name.replace('-', "_"))
}

/// Compile one package's doctests and read back what they used.
///
/// The gather runs per package because rustdoc feeds each doctest to the test
/// builder on stdin, so the shim cannot tell which crate a snippet came from.
/// Cargo can: one invocation per package attributes the captures for us.
///
/// # Errors
///
/// Returns an error when cargo cannot be launched, the doctest build fails, or
/// the capture file cannot be read.
pub fn gather_package(manifest_path: &Path, package: &str, target_dir: &Path, shim_path: &Path) -> Result<PackageDoctests> {
    gather_package_with(
        manifest_path,
        package,
        target_dir,
        shim_path,
        selected_cargo(|key| std::env::var_os(key)),
        selected_rustdoc,
        |mut command| command.output(),
    )
}

fn gather_package_with(
    manifest_path: &Path,
    package: &str,
    target_dir: &Path,
    shim_path: &Path,
    cargo: OsString,
    select_rustdoc: impl FnOnce() -> Result<OsString>,
    run: impl FnOnce(Command) -> std::io::Result<Output>,
) -> Result<PackageDoctests> {
    std::fs::create_dir_all(target_dir).into_app_err(format!("failed to create {}", target_dir.display()))?;

    let capture = target_dir.join(format!("doctests-{package}"));
    clear_capture(&capture)?;
    create_capture(&capture, |path| std::fs::create_dir(path))?;
    let rustdoc = select_rustdoc()?;

    let mut command = Command::new(cargo);
    command
        .arg("test")
        .arg("--doc")
        .arg("--manifest-path")
        .arg(manifest_path)
        .arg("--package")
        .arg(package)
        .arg("--all-features")
        .arg("--target-dir")
        .arg(target_dir)
        .env("RUSTDOC", shim_path)
        .env(INNER_RUSTDOC_VAR, rustdoc)
        .env(CAPTURE_VAR, &capture);
    // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, reason = "dispatch observes only var_os(...).is_some() for this marker and never reads its value")]
    command.env(RUSTDOC_WRAPPER_VAR, "1");
    let output = run(command).into_app_err("failed to run `cargo test --doc` to collect doctest evidence")?;

    if !output.status.success() {
        ohno::bail!(
            "`cargo test --doc` failed while collecting doctest evidence for {package}:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    read_captures(&capture)
}

/// Resolve the rustdoc that ships with the active rustc before replacing it.
///
/// The sysroot is asked of rustc rather than of rustup because not every
/// toolchain is managed by rustup, and it keeps rustdoc from the same toolchain
/// as the compiler the shim runs.
fn selected_rustdoc() -> Result<OsString> {
    selected_rustdoc_with(|key| std::env::var_os(key), |mut command| command.output())
}

fn selected_rustdoc_with(
    mut lookup: impl FnMut(&str) -> Option<OsString>,
    run: impl FnOnce(Command) -> std::io::Result<Output>,
) -> Result<OsString> {
    if let Some(rustdoc) = lookup("RUSTDOC") {
        return Ok(rustdoc);
    }
    let rustc = selected_rustc(lookup);
    let mut command = Command::new(rustc);
    command.args(["--print", "sysroot"]);
    let output = run(command).into_app_err("failed to ask rustc for its sysroot")?;
    rustdoc_in_sysroot(output.status.success(), &output.stdout, &output.stderr)
}

fn rustdoc_in_sysroot(success: bool, stdout: &[u8], stderr: &[u8]) -> Result<OsString> {
    if !success {
        ohno::bail!("rustc could not report its sysroot:\n{}", String::from_utf8_lossy(stderr).trim());
    }
    let sysroot = String::from_utf8_lossy(stdout).trim().to_owned();
    if sysroot.is_empty() {
        ohno::bail!("rustc reported an empty sysroot");
    }
    Ok(Path::new(&sysroot)
        .join("bin")
        .join(format!("rustdoc{}", std::env::consts::EXE_SUFFIX))
        .into_os_string())
}

/// Use an environment-selected tool or its conventional executable name.
fn tool_or_default(selected: Option<OsString>, default: &str) -> OsString {
    selected.unwrap_or_else(|| OsString::from(default))
}

fn selected_rustc(lookup: impl FnOnce(&str) -> Option<OsString>) -> OsString {
    tool_or_default(lookup("RUSTC"), "rustc")
}

fn selected_cargo(lookup: impl FnOnce(&str) -> Option<OsString>) -> OsString {
    tool_or_default(lookup("CARGO"), "cargo")
}

/// Remove stale captures from an earlier run.
fn clear_capture(capture: &Path) -> Result<()> {
    if capture.exists() {
        std::fs::remove_dir_all(capture).into_app_err(format!("failed to clear {}", capture.display()))?;
    }
    Ok(())
}

fn create_capture(capture: &Path, create: impl FnOnce(&Path) -> std::io::Result<()>) -> Result<()> {
    create(capture).into_app_err(format!("failed to create {}", capture.display()))?;
    Ok(())
}

/// Read the capture file the shim wrote for one package.
fn read_captures(capture: &Path) -> Result<PackageDoctests> {
    if !capture.exists() {
        return Ok(PackageDoctests::default());
    }

    let entries = std::fs::read_dir(capture).into_app_err(format!("failed to read {}", capture.display()))?;
    read_capture_entries(capture, entries.map(|entry| entry.map(|entry| entry.path())))
}

fn read_capture_entries(capture: &Path, entries: impl IntoIterator<Item = std::io::Result<std::path::PathBuf>>) -> Result<PackageDoctests> {
    let mut doctests = PackageDoctests::default();
    for entry in entries {
        let path = entry.into_app_err(format!("failed to enumerate {}", capture.display()))?;
        let text = std::fs::read_to_string(&path).into_app_err(format!("failed to read {}", path.display()))?;
        for line in text.lines() {
            match line.strip_prefix('\t') {
                Some(name) => {
                    *doctests.reports.entry(name.to_owned()).or_default() += 1;
                }
                None => doctests.compiled += 1,
            }
        }
    }

    Ok(doctests)
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt as _;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt as _;
    use std::path::Path;

    use tempfile::TempDir;

    use super::{
        DoctestEvidence, PackageDoctests, capture_compiler_output, clear_capture, gather_package_with, read_captures, record,
        rustdoc_in_sysroot, rustdoc_wrapper_with, selected_cargo, selected_rustc, selected_rustdoc_with, stable_diagnostic_args,
        tool_or_default, unused_name,
    };

    fn compiler_output(success: bool, stdout: &str, stderr: &str) -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw((!success).into()),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn compiler_capture_propagates_spawn_record_and_diagnostic_write_failures() {
        struct FailingWriter;
        impl std::io::Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("diagnostics denied"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let directory = TempDir::new().expect("test workspace exists");
        let error = capture_compiler_output(directory.path(), Err(std::io::Error::other("spawn denied")), &mut Vec::new())
            .expect_err("a compiler spawn failure is returned");
        assert!(error.to_string().contains("failed to run rustc from the doctest shim"), "{error}");
        let error = capture_compiler_output(
            &directory.path().join("missing"),
            Ok(compiler_output(true, "", "diagnostic")),
            &mut Vec::new(),
        )
        .expect_err("recording into a missing directory fails");
        assert!(error.to_string().contains("failed to create the doctest capture record"), "{error}");

        let error = capture_compiler_output(directory.path(), Ok(compiler_output(false, "", "diagnostic")), &mut FailingWriter)
            .expect_err("diagnostic forwarding failures are returned");
        assert!(error.to_string().contains("failed to forward rustc diagnostics"), "{error}");
        let mut forwarded = Vec::new();
        assert_eq!(
            capture_compiler_output(directory.path(), Ok(compiler_output(false, "", "compiler failure")), &mut forwarded)
                .expect("failed compilation is still a recorded result"),
            std::process::ExitCode::FAILURE,
        );
        assert_eq!(forwarded, b"compiler failure");
        assert_eq!(read_captures(directory.path()).expect("captures are readable").compiled, 2);
    }

    #[test]
    fn rustdoc_wrapper_preserves_tool_arguments_and_both_error_contexts() {
        let error = rustdoc_wrapper_with(&[], None, Err(std::io::Error::other("executable unavailable")), |_| {
            panic!("must not spawn")
        })
        .expect_err("current executable failure must propagate");
        assert!(
            error
                .to_string()
                .contains("failed to locate this executable to use as the doctest shim"),
            "{error}"
        );
        for selected in [None, Some(OsString::from("custom-rustdoc"))] {
            let expected = selected.clone().unwrap_or_else(|| "rustdoc".into());
            let error = rustdoc_wrapper_with(&["input.rs".into()], selected, Ok("shim".into()), |command| {
                assert_eq!(command.get_program(), expected);
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    ["input.rs", "-Z", "unstable-options", "--no-run", "--test-builder", "shim"]
                );
                assert!(
                    command
                        .get_envs()
                        .any(|(key, value)| key == super::RUSTDOC_WRAPPER_VAR && value.is_none())
                );
                Err(std::io::Error::other("rustdoc unavailable"))
            })
            .expect_err("rustdoc spawn failure must propagate");
            assert!(
                error.to_string().contains("failed to run rustdoc from the doctest wrapper"),
                "{error}"
            );
        }
    }

    #[test]
    fn selected_tools_use_exact_environment_keys_and_conventional_defaults() {
        assert_eq!(
            selected_rustc(|key| {
                assert_eq!(key, "RUSTC");
                Some("custom-compiler".into())
            }),
            "custom-compiler"
        );
        assert_eq!(selected_rustc(|_| None), "rustc");
        assert_eq!(
            selected_cargo(|key| {
                assert_eq!(key, "CARGO");
                Some("custom-cargo".into())
            }),
            "custom-cargo"
        );
        assert_eq!(selected_cargo(|_| None), "cargo");
        assert_eq!(
            selected_rustdoc_with(
                |key| {
                    assert_eq!(key, "RUSTDOC");
                    Some("custom-doc".into())
                },
                |_| panic!("override needs no compiler")
            )
            .expect("explicit rustdoc is selected"),
            "custom-doc",
        );
        let error = selected_rustdoc_with(
            |key| match key {
                "RUSTDOC" => None,
                "RUSTC" => Some("custom-compiler".into()),
                _ => panic!("unexpected environment key: {key}"),
            },
            |command| {
                assert_eq!(command.get_program(), "custom-compiler");
                assert_eq!(command.get_args().collect::<Vec<_>>(), ["--print", "sysroot"]);
                Err(std::io::Error::other("compiler unavailable"))
            },
        )
        .expect_err("sysroot discovery failure is returned");
        assert!(error.to_string().contains("failed to ask rustc for its sysroot"), "{error}");
    }

    #[test]
    fn doctest_collection_forwards_every_argument_and_error_stage() {
        let directory = TempDir::new().expect("test workspace exists");
        let target = directory.path().join("target");
        let error = gather_package_with(
            Path::new("Cargo.toml"),
            "fixture@1",
            &target,
            Path::new("shim"),
            "custom-cargo".into(),
            || Ok("selected-rustdoc".into()),
            |command| {
                assert_eq!(command.get_program(), "custom-cargo");
                let expected: Vec<OsString> = [
                    "test",
                    "--doc",
                    "--manifest-path",
                    "Cargo.toml",
                    "--package",
                    "fixture@1",
                    "--all-features",
                    "--target-dir",
                ]
                .into_iter()
                .map(OsString::from)
                .chain([target.as_os_str().to_owned()])
                .collect();
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    expected.iter().map(OsString::as_os_str).collect::<Vec<_>>()
                );
                let environment: std::collections::BTreeMap<_, _> = command
                    .get_envs()
                    .map(|(key, value)| (key.to_owned(), value.map(std::ffi::OsStr::to_owned)))
                    .collect();
                assert_eq!(environment.get(std::ffi::OsStr::new("RUSTDOC")), Some(&Some("shim".into())));
                assert_eq!(
                    environment.get(std::ffi::OsStr::new(super::INNER_RUSTDOC_VAR)),
                    Some(&Some("selected-rustdoc".into()))
                );
                assert!(
                    environment
                        .get(std::ffi::OsStr::new(super::RUSTDOC_WRAPPER_VAR))
                        .is_some_and(Option::is_some)
                );
                assert_eq!(
                    environment.get(std::ffi::OsStr::new(super::CAPTURE_VAR)),
                    Some(&Some(target.join("doctests-fixture@1").into_os_string()))
                );
                Err(std::io::Error::other("cargo unavailable"))
            },
        )
        .expect_err("cargo spawn errors must propagate");
        assert!(
            error
                .to_string()
                .contains("failed to run `cargo test --doc` to collect doctest evidence"),
            "{error}"
        );
        let error = gather_package_with(
            Path::new("Cargo.toml"),
            "fixture@1",
            &target,
            Path::new("shim"),
            "cargo".into(),
            || Err(ohno::app_err!("rustdoc selection failed")),
            |_| panic!("must not run Cargo"),
        )
        .expect_err("rustdoc selection failure must propagate");
        assert!(error.to_string().contains("rustdoc selection failed"), "{error}");
        let blocked = directory.path().join("file");
        fs::write(&blocked, "").expect("blocking file exists");
        gather_package_with(
            Path::new("Cargo.toml"),
            "fixture@1",
            &blocked,
            Path::new("shim"),
            "cargo".into(),
            || panic!("must not select rustdoc"),
            |_| panic!("must not run Cargo"),
        )
        .expect_err("target creation failure must propagate");
        fs::create_dir_all(&target).expect("target exists");
        fs::write(target.join("doctests-blocked"), "").expect("blocking capture exists");
        gather_package_with(
            Path::new("Cargo.toml"),
            "blocked",
            &target,
            Path::new("shim"),
            "cargo".into(),
            || panic!("must not select rustdoc"),
            |_| panic!("must not run Cargo"),
        )
        .expect_err("capture clear failure must propagate");
    }

    #[test]
    fn diagnostic_flag_forms_are_removed_without_touching_similar_inputs() {
        let args: Vec<OsString> = [
            "--color=always",
            "--color",
            "never",
            "--error-format",
            "json",
            "--json",
            "artifacts",
            "--error-format=human",
            "--json=diagnostic-short",
            "contains--color=always",
            "--colors=always",
            "input.rs",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        assert_eq!(
            stable_diagnostic_args(&args),
            ["contains--color=always", "--colors=always", "input.rs"].map(OsString::from)
        );
    }

    #[test]
    fn malformed_lint_names_never_panic_or_accept_unrelated_diagnostics() {
        for text in [
            "error: unrelated `dep`",
            "warning: extern crate `unterminated",
            "error: extern crate `unterminated",
            "note: extern crate `dep` is unused",
        ] {
            assert_eq!(unused_name(text), None, "{text}");
        }
    }

    #[test]
    fn capture_paths_propagate_create_read_and_clear_failures() {
        let directory = TempDir::new().expect("test workspace can be created");
        let absent = directory.path().join("absent");
        let error = record(&absent, "").expect_err("a missing capture directory is an error");
        assert!(error.to_string().contains("failed to create the doctest capture record"), "{error}");
        let file = directory.path().join("file");
        fs::write(&file, "").expect("test file can be created");
        let error = read_captures(&file).expect_err("a file cannot be enumerated as a directory");
        assert!(error.to_string().contains("failed to read"), "{error}");
        let error = clear_capture(&file).expect_err("a file cannot be cleared as a directory");
        assert!(error.to_string().contains("failed to clear"), "{error}");
        let captures = directory.path().join("captures");
        fs::create_dir(&captures).expect("capture directory can be created");
        fs::create_dir(captures.join("not-a-record")).expect("invalid record can be created");
        let error = read_captures(&captures).expect_err("a directory cannot be read as record text");
        assert!(error.to_string().contains("failed to read"), "{error}");
    }

    #[test]
    fn capture_creation_and_entry_enumeration_errors_preserve_capture_context() {
        let capture = Path::new("capture");
        let error = super::create_capture(capture, |path| {
            assert_eq!(path, capture);
            Err(std::io::Error::other("creation failed"))
        })
        .expect_err("capture creation failures propagate");
        assert!(error.to_string().contains("failed to create capture"), "{error}");
        let error = super::read_capture_entries(capture, [Err(std::io::Error::other("entry unavailable"))])
            .expect_err("entry enumeration failures propagate");
        assert!(error.to_string().contains("failed to enumerate capture"), "{error}");
    }

    #[test]
    fn capture_sequence_uses_every_identifier_before_wrapping() {
        let sequence = std::sync::atomic::AtomicUsize::new(usize::MAX - 1);
        assert_eq!(super::next_capture_sequence(&sequence), usize::MAX - 1);
        assert_eq!(super::next_capture_sequence(&sequence), usize::MAX);
        assert_eq!(super::next_capture_sequence(&sequence), 0);
        assert_eq!(super::next_capture_sequence(&sequence), 1);
    }

    #[test]
    fn lint_diagnostics_yield_normalized_dependency_names() {
        assert_eq!(unused_name("warning: extern crate `my-dep` is unused"), Some("my_dep".to_owned()));
        assert_eq!(unused_name("error: extern crate `broken` is unused"), Some("broken".to_owned()));
        assert_eq!(unused_name("warning: something else"), None);
        assert_eq!(unused_name("warning: extern crate without backticks"), None);
    }

    #[test]
    fn diagnostic_flags_are_normalized_without_dropping_compiler_inputs() {
        let args = [
            "input.rs".into(),
            "--error-format=json".into(),
            "--color".into(),
            "always".into(),
            "--json=diagnostic-rendered-ansi".into(),
            "--cfg".into(),
            "feature=\"x\"".into(),
        ];

        assert_eq!(
            stable_diagnostic_args(&args),
            [OsString::from("input.rs"), OsString::from("--cfg"), OsString::from("feature=\"x\"")]
        );
    }

    #[test]
    fn captures_count_each_compilation_and_report() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().join("capture");
        fs::create_dir(&path).expect("failed to create capture dir");

        record(
            &path,
            "warning: extern crate `unused` is unused\nwarning: extern crate `also-unused` is unused",
        )
        .expect("first capture is recorded");
        record(&path, "warning: extern crate `unused` is unused").expect("second capture is recorded");

        let captures = read_captures(&path).expect("captures are readable");
        assert_eq!(captures.compiled, 2);
        assert_eq!(captures.reports.get("unused"), Some(&2));
        assert_eq!(captures.reports.get("also_unused"), Some(&1));
    }

    #[test]
    fn a_missing_capture_is_empty() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let captures = read_captures(&dir.path().join("missing.tsv")).expect("a missing capture is valid");

        assert_eq!(captures.compiled, 0);
        assert!(captures.reports.is_empty());
    }

    #[test]
    fn stale_captures_are_cleared() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().join("capture");
        fs::create_dir(&path).expect("failed to create capture dir");
        fs::write(path.join("stale.tsv"), "stale").expect("failed to seed capture");

        clear_capture(&path).expect("capture is cleared");
        assert!(!path.exists());
        clear_capture(&path).expect("an absent capture is already clear");
    }

    #[test]
    fn tool_selection_honors_an_override_and_has_a_default() {
        assert_eq!(tool_or_default(Some("custom".into()), "rustc"), "custom");
        assert_eq!(tool_or_default(None, "rustc"), "rustc");
    }

    #[test]
    fn rustdoc_is_found_in_a_nonempty_sysroot() {
        let failed = rustdoc_in_sysroot(false, b"", b"no toolchain").expect_err("a failed rustc invocation is an error");
        assert!(failed.to_string().contains("no toolchain"), "unexpected error: {failed}");
        rustdoc_in_sysroot(true, b"  \n", b"").expect_err("an empty sysroot is an error");
        assert_eq!(
            rustdoc_in_sysroot(true, b"toolchain\n", b"").expect("a sysroot is valid"),
            Path::new("toolchain")
                .join("bin")
                .join(format!("rustdoc{}", std::env::consts::EXE_SUFFIX))
                .into_os_string()
        );
    }

    #[test]
    fn doctest_use_requires_fewer_reports_than_compilations() {
        let mut evidence = DoctestEvidence::default();
        evidence.insert(
            "fixture/Cargo.toml".into(),
            PackageDoctests {
                compiled: 2,
                reports: [("sometimes".to_owned(), 1), ("never".to_owned(), 2)].into_iter().collect(),
            },
        );

        assert!(evidence.used(Path::new("fixture/Cargo.toml"), "sometimes"));
        assert!(!evidence.used(Path::new("fixture/Cargo.toml"), "never"));
        assert!(!evidence.used(Path::new("missing/Cargo.toml"), "sometimes"));
    }

    #[test]
    fn malformed_capture_text_is_still_counted_conservatively() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().join("capture");
        fs::create_dir(&path).expect("failed to create capture dir");
        fs::write(path.join("record.tsv"), "compiled\n\tunused\n").expect("failed to seed capture");

        let captures = read_captures(&path).expect("capture is readable");
        assert_eq!(captures.compiled, 1);
        assert_eq!(captures.reports.get("unused"), Some(&1));
    }
}
