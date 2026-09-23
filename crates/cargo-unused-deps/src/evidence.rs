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
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, bail};

/// The diagnostic this tool listens for.
const LINT: &str = "unused_crate_dependencies";

/// Marks an invocation of this binary as Cargo's workspace rustc wrapper.
pub const WRAPPER_VAR: &str = "CARGO_UNUSED_DEPS_RUSTC_WRAPPER";

/// Preserves a caller-provided workspace wrapper behind this tool's wrapper.
const INNER_WRAPPER_VAR: &str = "CARGO_UNUSED_DEPS_INNER_RUSTC_WRAPPER";

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
    /// Whether the *plain* library or binary unit of any code target loaded
    /// `name`.
    ///
    /// This must be read from a run that built default targets only, where each
    /// code target has exactly one unit and a report can only have come from it.
    /// Reading it from an `--all-targets` run would be guesswork: the plain and
    /// `cfg(test)` units of a target produce indistinguishable diagnostics, and
    /// `#[cfg(not(test))]` code means the `cfg(test)` unit is not a superset of
    /// the plain one, so "exactly one report must be the plain unit's" does not
    /// hold.
    pub fn used_by_plain_unit(&self, manifest_path: &Path, name: &str) -> bool {
        self.targets_of(manifest_path, TargetKind::Code)
            .any(|target| self.reports_for(target, name) == 0)
    }

    /// Whether any unit that had `name` in scope loaded it.
    ///
    /// This one needs no assumption about which unit spoke: a unit reports
    /// exactly when it had the dependency in scope and did not use it, so fewer
    /// reports than in-scope units means some in-scope unit used it.
    pub fn used_by_any_unit(&self, manifest_path: &Path, name: &str, scope: Scope) -> bool {
        let in_code_target = self.targets_of(manifest_path, TargetKind::Code).any(|target| {
            let units = self.units.get(target).copied().unwrap_or_default();
            let reports = self.reports_for(target, name);

            match scope {
                // A normal dependency is in scope for every unit of the target.
                Scope::Always => reports < units,

                // A dev-dependency is in scope only for the `cfg(test)` unit, so
                // that unit is the only one that can report, and any report is
                // its report. It exists only when the target was compiled twice.
                Scope::DevelopmentOnly => units >= 2 && reports == 0,
            }
        });

        in_code_target
            || self.targets_of(manifest_path, TargetKind::Development).any(|target| {
                // Test, bench and example targets have both kinds of dependency
                // in scope for every unit, and are themselves compiled twice
                // when declared `test = true`.
                let units = self.units.get(target).copied().unwrap_or_default();

                self.reports_for(target, name) < units
            })
    }

    /// Whether the build script loaded `name`.
    pub fn used_by_build_script(&self, manifest_path: &Path, name: &str) -> bool {
        self.targets_of(manifest_path, TargetKind::Build)
            .any(|target| self.reports_for(target, name) == 0)
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
/// emits a malformed JSON message line. Non-message output is ignored.
pub fn gather(manifest_path: &Path, selection: &[OsString], target_dir: &Path, all_targets: bool) -> Result<Evidence> {
    let mut command = Command::new(cargo());
    command.arg("check").arg("--manifest-path").arg(manifest_path).args(selection);
    let wrapper = std::env::current_exe().context("failed to locate this executable to use as the rustc wrapper")?;
    ensure_no_configured_workspace_wrapper(manifest_path)?;
    if let Some(inner) = std::env::var_os("RUSTC_WORKSPACE_WRAPPER") {
        command.env(INNER_WRAPPER_VAR, inner);
    }
    command.env("RUSTC_WORKSPACE_WRAPPER", wrapper).env(WRAPPER_VAR, "1");

    if all_targets {
        command.arg("--all-targets");
    }

    let output = command
        .arg("--all-features")
        .arg("--target-dir")
        .arg(target_dir)
        .arg("--message-format=json")
        .output()
        .context("failed to run `cargo check` to collect compile evidence")?;

    if !output.status.success() {
        bail!(
            "`cargo check` failed while collecting compile evidence:\n{}",
            failure_diagnostics(&output.stdout, &output.stderr)
        );
    }

    parse(&String::from_utf8_lossy(&output.stdout))
}

/// Refuse to replace a Cargo-configured workspace wrapper we cannot chain safely.
fn ensure_no_configured_workspace_wrapper(manifest_path: &Path) -> Result<()> {
    let directory = manifest_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let output = Command::new(cargo())
        .args(["-Z", "unstable-options", "config", "get"])
        .current_dir(directory)
        .output()
        .context("failed to inspect Cargo compiler-wrapper configuration")?;
    if !output.status.success() {
        bail!(
            "failed to inspect Cargo compiler-wrapper configuration:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if has_workspace_wrapper_config(&String::from_utf8_lossy(&output.stdout)) {
        bail!("build.rustc-workspace-wrapper is configured; cargo-unused-deps cannot safely interpose without bypassing it");
    }
    Ok(())
}

fn has_workspace_wrapper_config(config: &str) -> bool {
    config
        .lines()
        .any(|line| line.trim_start().starts_with("build.rustc-workspace-wrapper ="))
}

/// Run as Cargo's rustc wrapper and make the evidence lint non-overridable.
///
/// Cargo has already resolved config and environment rustflags before invoking
/// the wrapper, so appending here preserves every active flag source. Cargo's
/// outer `RUSTC_WRAPPER`/`CARGO_BUILD_RUSTC_WRAPPER` remains outside this
/// workspace wrapper, while an existing workspace wrapper is chained inside it.
pub fn wrapper(args: &[OsString]) -> Result<ExitCode> {
    let (rustc, rustc_args) = args.split_first().context("rustc wrapper was invoked without a compiler path")?;
    let mut command = wrapper_command(std::env::var_os(INNER_WRAPPER_VAR), rustc);
    let status = command
        .args(rustc_args)
        .args(["--force-warn", LINT])
        .env_remove(WRAPPER_VAR)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run rustc from the compile-evidence wrapper")?;

    Ok(if status.success() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// Construct the wrapper chain before appending rustc's ordinary arguments.
fn wrapper_command(inner: Option<OsString>, rustc: &OsString) -> Command {
    match inner {
        Some(wrapper) => {
            let mut command = Command::new(wrapper);
            command.arg(rustc);
            command
        }
        None => Command::new(rustc),
    }
}

/// Render compiler diagnostics from Cargo's JSON stream alongside Cargo errors.
fn failure_diagnostics(stdout: &[u8], stderr: &[u8]) -> String {
    let rendered = String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|message| {
            let diagnostic = message.get("message")?;
            (diagnostic.get("level").and_then(serde_json::Value::as_str) == Some("error"))
                .then(|| diagnostic.get("rendered").and_then(serde_json::Value::as_str).map(str::to_owned))
                .flatten()
        })
        .collect::<String>();
    let cargo_errors = String::from_utf8_lossy(stderr);

    match (rendered.trim(), cargo_errors.trim()) {
        ("", "") => "cargo failed without diagnostics".to_owned(),
        ("", errors) => errors.to_owned(),
        (diagnostics, "") => diagnostics.to_owned(),
        (diagnostics, errors) => format!("{diagnostics}\n{errors}"),
    }
}

/// The cargo to invoke, honoring the one cargo set for us.
fn cargo() -> OsString {
    cargo_or_default(std::env::var_os("CARGO"))
}

/// Use Cargo selected by the environment or its conventional executable name.
fn cargo_or_default(selected: Option<OsString>) -> OsString {
    selected.unwrap_or_else(|| OsString::from("cargo"))
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::{
        Evidence, Scope, TargetKind, cargo_or_default, failure_diagnostics, has_workspace_wrapper_config, parse, reported_name,
        wrapper_command,
    };

    #[test]
    fn target_kinds_are_classified_explicitly() {
        assert_eq!(TargetKind::classify("custom-build"), Some(TargetKind::Build));
        assert_eq!(TargetKind::classify("test"), Some(TargetKind::Development));
        assert_eq!(TargetKind::classify("lib"), Some(TargetKind::Code));
        assert_eq!(TargetKind::classify("unknown"), None);
    }

    #[test]
    fn cargo_selection_honors_an_override_and_has_a_default() {
        assert_eq!(cargo_or_default(Some("custom".into())), "custom");
        assert_eq!(cargo_or_default(None), "cargo");
    }

    #[test]
    fn compiler_wrapper_chains_an_existing_wrapper() {
        let rustc = OsString::from("rustc");
        let direct = wrapper_command(None, &rustc);
        assert_eq!(direct.get_program(), "rustc");
        assert_eq!(direct.get_args().count(), 0);

        let chained = wrapper_command(Some("sccache".into()), &rustc);
        assert_eq!(chained.get_program(), "sccache");
        assert_eq!(chained.get_args().collect::<Vec<_>>(), ["rustc"]);
    }

    #[test]
    fn configured_workspace_wrappers_are_detected() {
        assert!(has_workspace_wrapper_config(
            "build.rustc-workspace-wrapper = \"workspace-wrapper\"\n"
        ));
        assert!(!has_workspace_wrapper_config("build.rustc-wrapper = \"outer-wrapper\"\n"));
    }

    #[test]
    fn parser_rejects_malformed_json_messages() {
        parse("{not json").expect_err("malformed JSON must be rejected");
    }

    #[test]
    fn parser_ignores_non_messages_and_unknown_targets() {
        let evidence = parse(
            "not json\n\
             {\"reason\":\"compiler-artifact\",\"manifest_path\":\"Cargo.toml\",\
             \"target\":{\"kind\":[\"unknown\"],\"name\":\"fixture\"}}\n\
             {\"reason\":\"build-script-executed\",\"manifest_path\":\"Cargo.toml\",\
             \"target\":{\"kind\":[\"lib\"],\"name\":\"fixture\"}}\n",
        )
        .expect("unknown target kinds are ignored");

        assert!(evidence.units.is_empty());
    }

    #[test]
    fn parser_counts_units_and_matching_lint_reports() {
        let target = "\"manifest_path\":\"Cargo.toml\",\"target\":{\"kind\":[\"lib\"],\"name\":\"fixture\"}";
        let evidence = parse(&format!(
            "{{\"reason\":\"compiler-artifact\",{target}}}\n\
             {{\"reason\":\"compiler-message\",{target},\"message\":{{\"code\":{{\"code\":\"unused_crate_dependencies\"}},\
             \"message\":\"warning: extern crate `unused` is unused\"}}}}\n"
        ))
        .expect("compiler messages are parsed");

        assert!(!evidence.units.is_empty());
        assert!(!evidence.used_by_any_unit(Path::new("Cargo.toml"), "unused", Scope::Always));
    }

    #[test]
    fn development_scope_requires_a_second_code_unit() {
        let target = "\"manifest_path\":\"Cargo.toml\",\"target\":{\"kind\":[\"lib\"],\"name\":\"fixture\"}";
        let one = parse(&format!("{{\"reason\":\"compiler-artifact\",{target}}}\n")).expect("one unit is parsed");
        let two = parse(&format!(
            "{{\"reason\":\"compiler-artifact\",{target}}}\n{{\"reason\":\"compiler-artifact\",{target}}}\n"
        ))
        .expect("two units are parsed");

        assert!(!one.used_by_any_unit(Path::new("Cargo.toml"), "devdep", Scope::DevelopmentOnly));
        assert!(two.used_by_any_unit(Path::new("Cargo.toml"), "devdep", Scope::DevelopmentOnly));
    }

    #[test]
    fn unrelated_compiler_messages_are_not_unused_dependency_reports() {
        let target = "\"manifest_path\":\"Cargo.toml\",\"target\":{\"kind\":[\"lib\"],\"name\":\"fixture\"}";
        let evidence = parse(&format!(
            "{{\"reason\":\"compiler-artifact\",{target}}}\n\
             {{\"reason\":\"compiler-message\",{target},\"message\":{{\"code\":{{\"code\":\"dead_code\"}},\
             \"message\":\"warning: extern crate `dep` is unused\"}}}}\n"
        ))
        .expect("compiler messages are parsed");

        assert!(evidence.used_by_any_unit(Path::new("Cargo.toml"), "dep", Scope::Always));
    }

    #[test]
    fn missing_evidence_never_claims_use() {
        let evidence = Evidence::default();

        assert!(!evidence.used_by_plain_unit(Path::new("Cargo.toml"), "dep"));
        assert!(!evidence.used_by_any_unit(Path::new("Cargo.toml"), "dep", Scope::Always));
        assert!(!evidence.used_by_build_script(Path::new("Cargo.toml"), "dep"));
    }

    #[test]
    fn diagnostic_names_are_read_only_from_the_expected_lint_shape() {
        let warning = serde_json::json!({"message": {"message": "warning: extern crate `my_dep` is unused"}});
        let malformed = serde_json::json!({"message": {"message": "warning without backticks"}});

        assert_eq!(reported_name(&warning).as_deref(), Some("my_dep"));
        assert_eq!(reported_name(&malformed), None);
        assert_eq!(reported_name(&serde_json::json!({})), None);
    }

    #[test]
    fn failed_builds_render_json_diagnostics_and_cargo_errors() {
        let stdout = concat!(
            "{\"reason\":\"compiler-message\",\"message\":{\"level\":\"warning\",\"rendered\":\"warning: unused\\n\"}}\n",
            "{\"reason\":\"compiler-message\",\"message\":{\"level\":\"error\",\"rendered\":\"error: bad source\\n\"}}",
        )
        .as_bytes();

        assert_eq!(
            failure_diagnostics(stdout, b"cargo: build failed\n"),
            "error: bad source\ncargo: build failed"
        );
        assert_eq!(failure_diagnostics(b"", b"cargo failed"), "cargo failed");
        assert_eq!(failure_diagnostics(stdout, b""), "error: bad source");
        assert_eq!(failure_diagnostics(b"", b""), "cargo failed without diagnostics");
    }
}
