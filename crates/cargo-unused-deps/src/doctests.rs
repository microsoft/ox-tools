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
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, bail};

/// Environment variable naming the file the shim appends its findings to.
///
/// Its presence is also what puts this binary into shim mode, since rustdoc
/// invokes a test builder as a bare rustc and passes no flag of our own.
pub const CAPTURE_VAR: &str = "CARGO_UNUSED_DEPS_CAPTURE";

/// What one package's doctests did and did not use.
#[derive(Debug, Default, Clone)]
pub struct PackageDoctests {
    /// How many doctests rustdoc compiled.
    compiled: usize,

    /// How many doctests reported each dependency unused.
    reports: BTreeMap<String, usize>,
}

/// Doctest evidence for a whole run, keyed by package name.
#[derive(Debug, Default)]
pub struct DoctestEvidence {
    /// Per-package findings.
    packages: BTreeMap<String, PackageDoctests>,
}

impl DoctestEvidence {
    /// Record one package's findings.
    pub fn insert(&mut self, package: String, doctests: PackageDoctests) {
        self.packages.insert(package, doctests);
    }

    /// Whether a doctest of `package` used `name`.
    ///
    /// Every doctest is a separate compilation with the dependency in scope, so
    /// the counting is the same as for targets: fewer reports than doctests
    /// means some doctest used it. Treating a single report as proof of disuse
    /// would convict a dependency that one example needs and the other 174 do
    /// not mention.
    pub fn used(&self, package: &str, name: &str) -> bool {
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
    let rustc = tool_or_default(std::env::var_os("RUSTC"), "rustc");
    let output = Command::new(&rustc)
        .args(args)
        .arg("-W")
        .arg("unused_crate_dependencies")
        // rustdoc feeds the doctest source on stdin; without this the shim
        // would hand rustc an empty program and every doctest would fail.
        .stdin(Stdio::inherit())
        .output()
        .context("failed to run rustc from the doctest shim")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    record(capture, &stderr)?;

    // rustdoc reads the compiler's stderr to report a failing doctest, so it is
    // passed through rather than swallowed. The error format is left alone for
    // the same reason: handing rustdoc JSON it did not ask for makes it treat
    // every doctest as failed.
    std::io::Write::write_all(&mut std::io::stderr(), &output.stderr).context("failed to forward rustc diagnostics")?;

    Ok(if output.status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Append the findings from one doctest to the capture file.
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

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(capture)
        .context(format!("failed to open the doctest capture file {}", capture.display()))?;

    file.write_all(lines.as_bytes())
        .context(format!("failed to write the doctest capture file {}", capture.display()))
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
    std::fs::create_dir_all(target_dir).context(format!("failed to create {}", target_dir.display()))?;

    let capture = target_dir.join(format!("doctests-{package}.tsv"));
    clear_capture(&capture)?;

    let mut rustdocflags = OsString::from("-Z unstable-options --no-run --test-builder ");
    rustdocflags.push(shim_path);

    let cargo = tool_or_default(std::env::var_os("CARGO"), "cargo");
    let output = Command::new(cargo)
        .arg("test")
        .arg("--doc")
        .arg("--manifest-path")
        .arg(manifest_path)
        .arg("--package")
        .arg(package)
        .arg("--all-features")
        .arg("--target-dir")
        .arg(target_dir)
        .env("RUSTDOCFLAGS", rustdocflags)
        .env(CAPTURE_VAR, &capture)
        .output()
        .context("failed to run `cargo test --doc` to collect doctest evidence")?;

    if !output.status.success() {
        bail!(
            "`cargo test --doc` failed while collecting doctest evidence for {package}:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    read_captures(&capture)
}

/// Use an environment-selected tool or its conventional executable name.
fn tool_or_default(selected: Option<OsString>, default: &str) -> OsString {
    selected.unwrap_or_else(|| OsString::from(default))
}

/// Remove stale captures from an earlier run.
fn clear_capture(capture: &Path) -> Result<()> {
    if capture.exists() {
        std::fs::remove_file(capture).context(format!("failed to clear {}", capture.display()))?;
    }
    Ok(())
}

/// Read the capture file the shim wrote for one package.
fn read_captures(capture: &Path) -> Result<PackageDoctests> {
    let mut doctests = PackageDoctests::default();
    if !capture.exists() {
        return Ok(doctests);
    }

    let text = std::fs::read_to_string(capture).context(format!("failed to read {}", capture.display()))?;
    for line in text.lines() {
        match line.strip_prefix('\t') {
            Some(name) => {
                *doctests.reports.entry(name.to_owned()).or_default() += 1;
            }
            None => doctests.compiled += 1,
        }
    }

    Ok(doctests)
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{DoctestEvidence, PackageDoctests, clear_capture, read_captures, record, tool_or_default, unused_name};

    #[test]
    fn lint_diagnostics_yield_normalized_dependency_names() {
        assert_eq!(unused_name("warning: extern crate `my-dep` is unused"), Some("my_dep".to_owned()));
        assert_eq!(unused_name("error: extern crate `broken` is unused"), Some("broken".to_owned()));
        assert_eq!(unused_name("warning: something else"), None);
        assert_eq!(unused_name("warning: extern crate without backticks"), None);
    }

    #[test]
    fn captures_count_each_compilation_and_report() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().join("capture.tsv");

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
        let path = dir.path().join("capture.tsv");
        fs::write(&path, "stale").expect("failed to seed capture");

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
    fn doctest_use_requires_fewer_reports_than_compilations() {
        let mut evidence = DoctestEvidence::default();
        evidence.insert(
            "fixture".to_owned(),
            PackageDoctests {
                compiled: 2,
                reports: [("sometimes".to_owned(), 1), ("never".to_owned(), 2)].into_iter().collect(),
            },
        );

        assert!(evidence.used("fixture", "sometimes"));
        assert!(!evidence.used("fixture", "never"));
        assert!(!evidence.used("missing", "sometimes"));
    }

    #[test]
    fn malformed_capture_text_is_still_counted_conservatively() {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = dir.path().join("capture.tsv");
        fs::write(&path, "compiled\n\tunused\n").expect("failed to seed capture");

        let captures = read_captures(&path).expect("capture is readable");
        assert_eq!(captures.compiled, 1);
        assert_eq!(captures.reports.get("unused"), Some(&1));
    }
}
