// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Compile evidence: which dependencies rustc actually loaded.
//!
//! The tool never parses Rust. It asks rustc, by enabling the
//! `unused_crate_dependencies` lint for its own build and reading the
//! diagnostics cargo forwards as JSON.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The diagnostic this tool listens for.
const LINT: &str = "unused_crate_dependencies";

/// Lint flag handed to every unit rustc compiles for us.
const LINT_FLAG: &str = "-W unused_crate_dependencies";

/// What a cargo target is, for the purpose of reading its evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TargetKind {
    /// A library or binary: compiled twice under `--all-targets`, once plainly
    /// and once with `cfg(test)`.
    Code,

    /// A test, bench or example target: compiled once, always for development.
    Development,

    /// A build script.
    Build,
}

impl TargetKind {
    /// Classify a cargo target kind, or `None` for kinds carrying no evidence.
    fn classify(kind: &str) -> Option<Self> {
        match kind {
            "custom-build" => Some(Self::Build),
            "test" | "bench" | "example" => Some(Self::Development),
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" | "bin" => Some(Self::Code),
            _ => None,
        }
    }
}

/// One cargo target, which may be compiled as more than one unit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Target {
    /// Manifest of the package the target belongs to.
    manifest_path: PathBuf,

    /// What kind of target it is.
    kind: TargetKind,

    /// The target's name, which distinguishes one binary or test from another.
    name: String,
}

/// Which units of a target a dependency was in scope for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Available to every unit: a normal dependency.
    Always,

    /// Available only where `cfg(test)` code is: a dev-dependency.
    DevelopmentOnly,
}

/// Everything the compiler told us about one selection of packages.
#[derive(Debug, Default)]
pub struct Evidence {
    /// How many units cargo compiled for each target.
    units: BTreeMap<Target, usize>,

    /// How many of those units reported a given dependency unused.
    reports: BTreeMap<(Target, String), usize>,
}

impl Evidence {
    /// Whether any *plain* library or binary unit loaded `name`.
    ///
    /// This is the question that decides whether a normal dependency earned its
    /// section, so it deliberately ignores `cfg(test)` code.
    pub fn used_by_library(&self, manifest_path: &Path, name: &str) -> bool {
        self.targets_of(manifest_path, TargetKind::Code)
            .any(|target| self.reports_for(target, name) == 0)
    }

    /// Whether any development unit loaded `name` — a test, bench or example
    /// target, or the `cfg(test)` unit of a library or binary.
    pub fn used_by_development(&self, manifest_path: &Path, name: &str, scope: Scope) -> bool {
        let in_code_target = self.targets_of(manifest_path, TargetKind::Code).any(|target| {
            let units = self.units.get(target).copied().unwrap_or_default();
            let reports = self.reports_for(target, name);

            // A target compiled once has no `cfg(test)` unit to testify.
            units >= 2
                && match scope {
                    // The test-profile unit compiles a superset of the plain
                    // unit's code, so when only one of them reports it can only
                    // be the plain one. Fewer reports than units therefore means
                    // the test-profile unit loaded the dependency.
                    Scope::Always => reports < units,

                    // A dev-dependency is in scope for that unit alone, so any
                    // report at all is its report.
                    Scope::DevelopmentOnly => reports == 0,
                }
        });

        in_code_target
            || self.targets_of(manifest_path, TargetKind::Development).any(|target| {
                // Development targets are usually compiled once, but a test,
                // bench or example declared `test = true` is compiled twice like
                // a library. The same monotonicity argument applies: fewer
                // reports than units means one of them loaded the dependency.
                // Both units have dev-dependencies in scope, so no distinction
                // by scope is needed here.
                let units = self.units.get(target).copied().unwrap_or_default();

                self.reports_for(target, name) < units
            })
    }

    /// Whether the build script loaded `name`.
    pub fn used_by_build_script(&self, manifest_path: &Path, name: &str) -> bool {
        self.targets_of(manifest_path, TargetKind::Build)
            .any(|target| self.reports_for(target, name) == 0)
    }

    /// Whether anything at all was compiled for a package.
    pub fn saw_package(&self, manifest_path: &Path) -> bool {
        self.units.keys().any(|target| target.manifest_path == manifest_path)
    }

    /// Targets of one kind belonging to a package.
    fn targets_of<'a>(&'a self, manifest_path: &'a Path, kind: TargetKind) -> impl Iterator<Item = &'a Target> {
        self.units
            .keys()
            .filter(move |target| target.manifest_path == manifest_path && target.kind == kind)
    }

    /// How many units of `target` reported `name` unused.
    fn reports_for(&self, target: &Target, name: &str) -> usize {
        self.reports.get(&(target.clone(), name.to_owned())).copied().unwrap_or_default()
    }
}

/// Run `cargo check` over `selection` with the lint enabled and collect what it says.
///
/// The lint level is set for this invocation only, never in the workspace's own
/// lint configuration: the raw lint fires per unit, so a correct workspace would
/// warn on every ordinary build, and under `-D warnings` it would fail.
///
/// # Errors
///
/// Returns an error when cargo cannot be launched, exits unsuccessfully, or
/// emits a line that is not the JSON it was asked for.
pub fn gather(manifest_path: &Path, selection: &[OsString], target_dir: &Path) -> Result<Evidence> {
    let output = Command::new(cargo())
        .arg("check")
        .arg("--manifest-path")
        .arg(manifest_path)
        .args(selection)
        .arg("--all-targets")
        .arg("--all-features")
        .arg("--target-dir")
        .arg(target_dir)
        .arg("--message-format=json")
        .env("RUSTFLAGS", rustflags())
        .output()
        .context("failed to run `cargo check` to collect compile evidence")?;

    if !output.status.success() {
        bail!(
            "`cargo check` failed while collecting compile evidence:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    parse(&String::from_utf8_lossy(&output.stdout))
}

/// The cargo to invoke, honoring the one cargo set for us.
fn cargo() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

/// `RUSTFLAGS` for the child build, preserving any the caller set.
fn rustflags() -> OsString {
    let mut flags = std::env::var_os("RUSTFLAGS").unwrap_or_default();
    if !flags.is_empty() {
        flags.push(" ");
    }
    flags.push(LINT_FLAG);
    flags
}

/// Read cargo's JSON stream into evidence.
fn parse(stdout: &str) -> Result<Evidence> {
    let mut evidence = Evidence::default();

    for line in stdout.lines().filter(|line| line.starts_with('{')) {
        let message: serde_json::Value = serde_json::from_str(line).context("cargo emitted a line that is not valid JSON")?;
        let Some(target) = target_of(&message) else {
            continue;
        };

        match message.get("reason").and_then(serde_json::Value::as_str) {
            Some("compiler-artifact") => *evidence.units.entry(target).or_default() += 1,
            Some("compiler-message") if is_lint(&message) => {
                if let Some(name) = reported_name(&message) {
                    *evidence.reports.entry((target, name)).or_default() += 1;
                }
            }
            _ => {}
        }
    }

    Ok(evidence)
}

/// The target a cargo message came from, when it identifies one.
fn target_of(message: &serde_json::Value) -> Option<Target> {
    let manifest_path = message.get("manifest_path").and_then(serde_json::Value::as_str)?;
    let target = message.get("target")?;
    let kind = target.get("kind")?.get(0)?.as_str()?;
    let name = target.get("name")?.as_str()?;

    Some(Target {
        manifest_path: PathBuf::from(manifest_path),
        kind: TargetKind::classify(kind)?,
        name: name.to_owned(),
    })
}

/// Whether a compiler message is the lint this tool listens for.
fn is_lint(message: &serde_json::Value) -> bool {
    message
        .get("message")
        .and_then(|inner| inner.get("code"))
        .and_then(|code| code.get("code"))
        .and_then(serde_json::Value::as_str)
        == Some(LINT)
}

/// The dependency named by an `extern crate ... is unused` diagnostic.
///
/// rustc states the name in prose rather than in a structured field, so it is
/// read back from between the first pair of backticks. The alternative,
/// `--json unused-externs`, collides with the `--message-format` cargo needs.
fn reported_name(message: &serde_json::Value) -> Option<String> {
    let text = message.get("message")?.get("message")?.as_str()?;
    let (_, after) = text.split_once('`')?;
    let (name, _) = after.split_once('`')?;

    Some(name.replace('-', "_"))
}
