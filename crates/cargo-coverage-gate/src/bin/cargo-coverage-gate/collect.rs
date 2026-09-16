// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Portable coverage collection for `cargo coverage-gate run`.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fs};

use cargo_metadata::{Metadata, MetadataCommand};
use ohno::{AppError, EnrichableExt as _, IntoAppError};
use semver::Version;

use crate::cli::{CollectionArgs, CoverageGateArgs, FeatureConfiguration};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MIN_CARGO_LLVM_COV_VERSION: &str = "0.9.0";

pub(crate) fn run(args: &CoverageGateArgs, collection: &CollectionArgs) -> Result<ExitCode, AppError> {
    if !args.lcov.is_empty() {
        return Err(AppError::new(
            "`--lcov` cannot be used with `cargo coverage-gate run`; collected LCOV paths are selected by `--coverage-dir`",
        ));
    }

    let tools = ToolPrograms::from_env();
    let workspace = WorkspaceInfo::load(&tools)?;
    let selection = Selection::resolve(&workspace, &args.packages)?;

    let mut collection = collection.clone();
    collection.coverage_dir = absolute_path(&collection.coverage_dir)?;
    let configurations = normalized_configurations(&collection.configurations);

    if let Some(target) = configured_no_coverage_target(&collection.no_coverage_targets, args.target.as_deref(), &tools)? {
        let result = format!("target `{target}` is configured for no coverage; tests passed without coverage collection or gating");
        run_plain_configurations(
            &workspace,
            &selection,
            &collection,
            &configurations,
            args.target.as_deref(),
            &tools,
            args.quiet,
        )?;
        crate::run::write_no_gate_summary(args, &result)?;
        eprintln!("coverage-gate: {result}");
        return Ok(ExitCode::SUCCESS);
    }

    validate_instrumentation_tools(&workspace, &tools)?;
    fs::create_dir_all(&collection.coverage_dir).into_app_err(format!(
        "failed to create coverage directory `{}`",
        collection.coverage_dir.display()
    ))?;

    let coverage_scratch = TemporaryDirectory::create(&workspace.target_dir.join("coverage-gate"))?;
    let coverage_target_dir = coverage_scratch.path().join("cargo-target");
    let execution = CollectionExecution {
        workspace: &workspace,
        selection: &selection,
        args: &collection,
        #[cfg(windows)]
        scratch_dir: coverage_scratch.path(),
        coverage_target_dir: &coverage_target_dir,
        target: args.target.as_deref(),
        tools: &tools,
        quiet: args.quiet,
    };
    let mut lcov_paths = Vec::with_capacity(configurations.len());
    for configuration in configurations {
        lcov_paths.push(collect_configuration(&execution, configuration)?);
    }

    let evaluation = crate::run::evaluate_paths(args, &lcov_paths, &selection.gated_names());
    combine_evaluation_and_cleanup(evaluation, coverage_scratch.cleanup()).complete()
}

struct FinalizedRun {
    result: Result<ExitCode, AppError>,
    cleanup_warning: Option<AppError>,
}

impl FinalizedRun {
    fn complete(self) -> Result<ExitCode, AppError> {
        if let Some(cleanup) = self.cleanup_warning {
            eprintln!("warning: coverage evaluation completed, but scratch cleanup failed: {cleanup}");
        }
        self.result
    }
}

fn combine_evaluation_and_cleanup(evaluation: Result<ExitCode, AppError>, cleanup: Result<(), AppError>) -> FinalizedRun {
    match (evaluation, cleanup) {
        (Err(evaluation), Ok(())) => FinalizedRun {
            result: Err(evaluation),
            cleanup_warning: None,
        },
        (Err(evaluation), Err(cleanup)) => FinalizedRun {
            result: Err(evaluation.enrich(format!("scratch cleanup also failed: {cleanup}"))),
            cleanup_warning: None,
        },
        (Ok(code), Ok(())) => FinalizedRun {
            result: Ok(code),
            cleanup_warning: None,
        },
        (Ok(code), Err(cleanup)) if code != ExitCode::SUCCESS => FinalizedRun {
            result: Ok(code),
            cleanup_warning: Some(cleanup),
        },
        (Ok(_), Err(cleanup)) => FinalizedRun {
            result: Err(cleanup.enrich("coverage evaluation passed, but scratch cleanup failed")),
            cleanup_warning: None,
        },
    }
}

struct CollectionExecution<'a> {
    workspace: &'a WorkspaceInfo,
    selection: &'a Selection,
    args: &'a CollectionArgs,
    #[cfg(windows)]
    scratch_dir: &'a Path,
    coverage_target_dir: &'a Path,
    target: Option<&'a str>,
    tools: &'a ToolPrograms,
    quiet: bool,
}

#[derive(Debug, Clone)]
struct ToolPrograms {
    cargo: OsString,
    rustc: OsString,
}

impl ToolPrograms {
    fn from_env() -> Self {
        Self {
            cargo: env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")),
            rustc: env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc")),
        }
    }

    fn cargo(&self) -> &OsStr {
        &self.cargo
    }

    fn rustc(&self) -> &OsStr {
        &self.rustc
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, AppError> {
    absolute_path_with(path, env::current_dir)
}

fn absolute_path_with(path: &Path, current_dir: impl FnOnce() -> io::Result<PathBuf>) -> Result<PathBuf, AppError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(current_dir().into_app_err("failed to resolve the current directory")?.join(path))
    }
}

#[derive(Debug)]
struct WorkspaceInfo {
    root: PathBuf,
    target_dir: PathBuf,
    members: Vec<WorkspaceMember>,
}

impl WorkspaceInfo {
    fn load(tools: &ToolPrograms) -> Result<Self, AppError> {
        let mut command = MetadataCommand::new();
        command.no_deps().cargo_path(tools.cargo());
        let metadata = command.exec().into_app_err("failed to load cargo workspace metadata")?;
        Ok(Self::from_metadata(&metadata))
    }

    fn from_metadata(metadata: &Metadata) -> Self {
        let mut members = metadata
            .workspace_packages()
            .iter()
            .map(|package| WorkspaceMember {
                name: package.name.to_string(),
                version: package.version.to_string(),
            })
            .collect::<Vec<_>>();
        members.sort_by(|left, right| left.name.cmp(&right.name).then_with(|| left.version.cmp(&right.version)));
        Self {
            root: metadata.workspace_root.clone().into_std_path_buf(),
            target_dir: metadata.target_directory.clone().into_std_path_buf(),
            members,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct WorkspaceMember {
    name: String,
    version: String,
}

impl WorkspaceMember {
    fn spec(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

#[derive(Debug)]
struct Selection {
    explicit: bool,
    members: Vec<WorkspaceMember>,
}

impl Selection {
    fn resolve(workspace: &WorkspaceInfo, package_selectors: &[String]) -> Result<Self, AppError> {
        if package_selectors.is_empty() {
            return Ok(Self {
                explicit: false,
                members: workspace.members.clone(),
            });
        }

        let mut selected = BTreeSet::new();
        for selector in package_selectors {
            let matches = workspace
                .members
                .iter()
                .filter(|member| selector_matches(selector, member))
                .cloned()
                .collect::<Vec<_>>();
            if matches.is_empty() {
                return Err(AppError::new(format!(
                    "`--package` selector `{selector}` did not match any workspace member"
                )));
            }
            selected.extend(matches);
        }

        Ok(Self {
            explicit: true,
            members: selected.into_iter().collect(),
        })
    }

    fn gated_names(&self) -> Vec<String> {
        if self.explicit {
            self.members.iter().map(|member| member.name.clone()).collect()
        } else {
            Vec::new()
        }
    }
}

fn selector_matches(selector: &str, member: &WorkspaceMember) -> bool {
    selector == member.spec() || glob_matches(selector, &member.name)
}

fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let name = name.chars().collect::<Vec<_>>();
    glob_matches_from(&pattern, &name)
}

fn glob_matches_from(pattern: &[char], name: &[char]) -> bool {
    let Some((&token, remaining_pattern)) = pattern.split_first() else {
        return name.is_empty();
    };
    match token {
        '?' => name
            .split_first()
            .is_some_and(|(_, remaining_name)| glob_matches_from(remaining_pattern, remaining_name)),
        '*' => {
            glob_matches_from(remaining_pattern, name)
                || name
                    .split_first()
                    .is_some_and(|(_, remaining_name)| glob_matches_from(pattern, remaining_name))
        }
        expected => name
            .split_first()
            .is_some_and(|(&actual, remaining_name)| expected == actual && glob_matches_from(remaining_pattern, remaining_name)),
    }
}

fn normalized_configurations(requested: &[FeatureConfiguration]) -> Vec<FeatureConfiguration> {
    if requested.is_empty() {
        return vec![FeatureConfiguration::AllFeatures, FeatureConfiguration::NoDefaultFeatures];
    }
    requested.iter().copied().collect::<BTreeSet<_>>().into_iter().collect()
}

impl FeatureConfiguration {
    fn cargo_flag(self) -> &'static str {
        match self {
            Self::AllFeatures => "--all-features",
            Self::NoDefaultFeatures => "--no-default-features",
        }
    }

    fn artifact_name(self) -> &'static str {
        match self {
            Self::AllFeatures => "all-features",
            Self::NoDefaultFeatures => "no-default",
        }
    }
}

fn configured_no_coverage_target(configured: &[String], target: Option<&str>, tools: &ToolPrograms) -> Result<Option<String>, AppError> {
    configured_no_coverage_target_with(configured, target, || resolve_rustc_host(tools))
}

fn configured_no_coverage_target_with(
    configured: &[String],
    target: Option<&str>,
    resolve_host: impl FnOnce() -> Result<String, AppError>,
) -> Result<Option<String>, AppError> {
    if configured.is_empty() {
        return Ok(None);
    }
    let effective = target.map(str::to_owned).map_or_else(resolve_host, Ok)?;
    Ok(configured.iter().any(|candidate| candidate == &effective).then_some(effective))
}

fn resolve_rustc_host(tools: &ToolPrograms) -> Result<String, AppError> {
    let mut rustc_version = Command::new(tools.rustc());
    rustc_version.arg("-vV");
    let rustc_version = read_stdout(&mut rustc_version, "rustc host-target discovery")?;
    rustc_host(&rustc_version)
        .map(str::to_owned)
        .ok_or_else(|| AppError::new("`rustc -vV` did not report a host target"))
}

fn validate_instrumentation_tools(workspace: &WorkspaceInfo, tools: &ToolPrograms) -> Result<(), AppError> {
    let mut cargo_version = cargo_command(workspace, tools);
    cargo_version.args(["--version", "--verbose"]);
    let cargo_version = read_stdout(&mut cargo_version, "cargo toolchain validation")?;
    let cargo_release =
        cargo_release(&cargo_version).ok_or_else(|| AppError::new("`cargo --version --verbose` did not report a release"))?;
    if !cargo_release.contains("-nightly") {
        return Err(AppError::new(format!(
            "`cargo coverage-gate run` requires nightly Cargo for `cfg(coverage_nightly)`, but the effective Cargo release is `{cargo_release}`"
        )));
    }

    let mut rustc_version = Command::new(tools.rustc());
    rustc_version.arg("-vV").current_dir(&workspace.root);
    let rustc_version = read_stdout(&mut rustc_version, "rustc toolchain validation")?;
    let rustc_release = rustc_release(&rustc_version).ok_or_else(|| AppError::new("`rustc -vV` did not report a release"))?;
    if !rustc_release.contains("-nightly") {
        return Err(AppError::new(format!(
            "`cargo coverage-gate run` requires nightly rustc for `cfg(coverage_nightly)`, but the effective rustc release is `{rustc_release}`"
        )));
    }

    let mut llvm_cov_version = cargo_command(workspace, tools);
    llvm_cov_version.args(["llvm-cov", "--version"]);
    let llvm_cov_version = read_stdout(&mut llvm_cov_version, "cargo-llvm-cov version validation")?;
    let version = cargo_llvm_cov_version(&llvm_cov_version)?;
    let minimum =
        Version::parse(MIN_CARGO_LLVM_COV_VERSION).expect("MIN_CARGO_LLVM_COV_VERSION is a compile-time semantic version literal");
    if !cargo_llvm_cov_is_supported(&version, &minimum) {
        return Err(AppError::new(format!(
            "`cargo coverage-gate run` requires cargo-llvm-cov >= {minimum}, but the effective version is {version}"
        )));
    }
    Ok(())
}

fn cargo_release(output: &str) -> Option<&str> {
    output.lines().find_map(|line| line.strip_prefix("release: "))
}

fn rustc_release(output: &str) -> Option<&str> {
    output.lines().find_map(|line| line.strip_prefix("release: "))
}

fn rustc_host(output: &str) -> Option<&str> {
    output.lines().find_map(|line| line.strip_prefix("host: "))
}

fn cargo_llvm_cov_is_supported(version: &Version, minimum: &Version) -> bool {
    version >= minimum
}

fn cargo_llvm_cov_version(output: &str) -> Result<Version, AppError> {
    let version = output
        .lines()
        .find_map(|line| {
            line.strip_prefix("cargo-llvm-cov ")
                .and_then(|value| value.split_whitespace().next())
        })
        .ok_or_else(|| AppError::new("`cargo llvm-cov --version` did not report a cargo-llvm-cov version"))?;
    Version::parse(version).into_app_err(format!("cargo-llvm-cov reported invalid semantic version `{version}`"))
}

fn read_stdout(command: &mut Command, description: &str) -> Result<String, AppError> {
    let display = command_display(command);
    command.stdout(Stdio::piped()).stderr(Stdio::inherit());
    let output = command.output().into_app_err(format!("failed to execute `{display}`"))?;
    if !output.status.success() {
        return Err(AppError::new(format!(
            "{description} failed: `{display}` exited with {}",
            output.status
        )));
    }
    String::from_utf8(output.stdout).into_app_err(format!("`{display}` output was not UTF-8"))
}

fn run_plain_configurations(
    workspace: &WorkspaceInfo,
    selection: &Selection,
    args: &CollectionArgs,
    configurations: &[FeatureConfiguration],
    target: Option<&str>,
    tools: &ToolPrograms,
    quiet: bool,
) -> Result<(), AppError> {
    for &configuration in configurations {
        let mut command = cargo_command(workspace, tools);
        command.args(["nextest", "run"]);
        append_package_selection(&mut command, selection);
        append_nextest_options(&mut command, args, configuration, target);
        command.arg("--no-tests=pass");
        if quiet {
            command.stdout(Stdio::null());
        }
        run_status(&mut command, "cargo nextest")?;
    }
    Ok(())
}

fn collect_configuration(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration) -> Result<PathBuf, AppError> {
    let lcov_path = execution
        .args
        .coverage_dir
        .join(format!("lcov-{}.info", configuration.artifact_name()));

    run_clean(execution.workspace, execution.coverage_target_dir, execution.tools, execution.quiet)?;
    run_nextest(execution, configuration)?;
    run_report(execution, configuration, &lcov_path)?;
    Ok(lcov_path)
}

fn cargo_command(workspace: &WorkspaceInfo, tools: &ToolPrograms) -> Command {
    let mut command = Command::new(tools.cargo());
    command.current_dir(&workspace.root);
    command
}

fn coverage_command(workspace: &WorkspaceInfo, coverage_target_dir: &Path, tools: &ToolPrograms) -> Command {
    let mut command = cargo_command(workspace, tools);
    command
        .env("CARGO_LLVM_COV_TARGET_DIR", coverage_target_dir)
        .env("CARGO_LLVM_COV_BUILD_DIR", coverage_target_dir);
    command
}

fn run_clean(workspace: &WorkspaceInfo, coverage_target_dir: &Path, tools: &ToolPrograms, quiet: bool) -> Result<(), AppError> {
    let mut command = coverage_command(workspace, coverage_target_dir, tools);
    command.args(["llvm-cov", "clean", "--workspace"]);
    if quiet {
        command.stdout(Stdio::null());
    }
    run_status(&mut command, "cargo llvm-cov clean")
}

fn run_nextest(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration) -> Result<(), AppError> {
    let mut command = coverage_command(execution.workspace, execution.coverage_target_dir, execution.tools);
    command.args(["llvm-cov", "nextest", "--no-report"]);
    append_package_selection(&mut command, execution.selection);
    append_nextest_options(&mut command, execution.args, configuration, execution.target);
    command.arg("--no-tests=pass");
    if execution.quiet {
        command.stdout(Stdio::null());
    }
    run_status(&mut command, "cargo llvm-cov nextest")
}

fn append_package_selection(command: &mut Command, selection: &Selection) {
    if selection.explicit {
        for member in &selection.members {
            command.arg("--package").arg(member.spec());
        }
    } else {
        command.arg("--workspace");
    }
}

fn append_nextest_options(command: &mut Command, args: &CollectionArgs, configuration: FeatureConfiguration, target: Option<&str>) {
    command.arg(configuration.cargo_flag()).arg("--locked");
    if let Some(target) = target {
        command.arg("--target").arg(target);
    }
    if let Some(jobs) = args.jobs {
        command.arg("--jobs").arg(jobs.get().to_string());
        command.arg("--build-jobs").arg(jobs.get().to_string());
    }
}

#[cfg(windows)]
// Linux mutation jobs cannot execute this Windows argument construction; the
// Windows response-file integration test verifies the resulting invocation.
#[mutants::skip]
fn prefixed_path_argument(prefix: &str, path: &Path) -> OsString {
    let mut argument = OsString::from(prefix);
    argument.push(path.as_os_str());
    argument
}

fn run_report(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration, lcov_path: &Path) -> Result<(), AppError> {
    #[cfg(not(windows))]
    let _ = configuration;
    let mut command = coverage_command(execution.workspace, execution.coverage_target_dir, execution.tools);
    command.args(["llvm-cov", "report", "--lcov", "--output-path"]).arg(lcov_path);
    append_package_selection(&mut command, execution.selection);
    if let Some(target) = execution.target {
        command.arg("--target").arg(target);
    }

    let display = command_display(&command);
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .into_app_err(format!("failed to execute `{display}`"))?;
    if output.status.success() {
        forward_output(&output, execution.quiet)?;
        return Ok(());
    }

    #[cfg(windows)]
    if let Some(arguments) = command_too_long_response_arguments(&String::from_utf8_lossy(&output.stderr)) {
        forward_output(&output, execution.quiet)?;
        eprintln!("coverage-gate: cargo-llvm-cov could not launch llvm-cov directly; retrying its export through an LLVM response file");
        return run_windows_report_fallback(execution, configuration, lcov_path, arguments);
    }

    if is_no_coverage_data(&String::from_utf8_lossy(&output.stderr)) {
        forward_stdout(&output, execution.quiet)?;
        fs::write(lcov_path, []).into_app_err(format!("failed to write empty LCOV file `{}`", lcov_path.display()))?;
        eprintln!("coverage-gate: cargo-llvm-cov found no coverable objects; evaluating an empty LCOV report");
        return Ok(());
    }

    forward_output(&output, execution.quiet)?;
    Err(AppError::new(format!(
        "cargo llvm-cov report failed: `{display}` exited with {}",
        output.status
    )))
}

fn forward_stdout(output: &Output, quiet: bool) -> Result<(), AppError> {
    if !quiet {
        io::stdout()
            .write_all(&output.stdout)
            .into_app_err("failed to forward cargo-llvm-cov stdout")?;
    }
    Ok(())
}

fn forward_output(output: &Output, quiet: bool) -> Result<(), AppError> {
    forward_stdout(output, quiet)?;
    io::stderr()
        .write_all(&output.stderr)
        .into_app_err("failed to forward cargo-llvm-cov stderr")
}

fn is_no_coverage_data(stderr: &str) -> bool {
    stderr.contains("no coverage data found") && stderr.contains("could not load coverage information")
}

#[cfg(any(windows, test))]
fn command_too_long_response_arguments(stderr: &str) -> Option<&str> {
    if !stderr.contains("(os error 206)") {
        return None;
    }
    let command = stderr
        .split_once("could not execute process `")?
        .1
        .split_once("` (never executed)")?
        .0;
    let arguments = command.split_once(" export ")?.1;
    (!arguments.is_empty()).then_some(arguments)
}

#[cfg(windows)]
// Linux mutation jobs cannot execute this Windows response-file boundary. The
// Windows integration test covers the fallback command and stable LCOV output.
#[mutants::skip]
// The spawned-binary integration test covers this fallback, but the outer
// coverage report cannot include that child binary's coverage object.
#[cfg_attr(coverage_nightly, coverage(off))]
fn run_windows_report_fallback(
    execution: &CollectionExecution<'_>,
    configuration: FeatureConfiguration,
    lcov_path: &Path,
    arguments: &str,
) -> Result<(), AppError> {
    let response = TemporaryPath::write(
        execution.scratch_dir,
        &format!("{}-objects.rsp", configuration.artifact_name()),
        arguments.as_bytes(),
    )?;
    let output = fs::File::create(lcov_path).into_app_err(format!(
        "failed to create LCOV file `{}` for response-file retry",
        lcov_path.display()
    ))?;
    let llvm_cov = discover_llvm_cov(execution.tools)?;
    let mut command = Command::new(llvm_cov);
    command
        .current_dir(&execution.workspace.root)
        .arg("export")
        .arg(prefixed_path_argument("@", response.path()))
        .stdout(Stdio::from(output));
    run_status(&mut command, "llvm-cov export response-file fallback")?;
    response.cleanup()
}

#[cfg(windows)]
// Linux mutation jobs cannot execute Windows LLVM discovery. The Windows
// integration fallback covers both the explicit override and discovered tool.
#[mutants::skip]
// Exercised through the spawned-binary fallback integration test.
#[cfg_attr(coverage_nightly, coverage(off))]
fn discover_llvm_cov(tools: &ToolPrograms) -> Result<PathBuf, AppError> {
    if let Some(cov) = env::var_os("LLVM_COV") {
        return Ok(PathBuf::from(cov));
    }

    let mut command = Command::new(tools.rustc());
    command.args(["--print", "target-libdir"]);
    let target_libdir = read_stdout(&mut command, "rustc LLVM-tool discovery")?;
    let rustlib = Path::new(target_libdir.trim())
        .parent()
        .ok_or_else(|| AppError::new("rustc target-libdir output had no parent directory"))?;
    Ok(rustlib.join("bin").join(format!("llvm-cov{}", env::consts::EXE_SUFFIX)))
}

fn run_status(command: &mut Command, description: &str) -> Result<(), AppError> {
    let display = command_display(command);
    let status = command.status().into_app_err(format!("failed to execute `{display}`"))?;
    if status.success() {
        Ok(())
    } else {
        Err(AppError::new(format!("{description} failed: `{display}` exited with {status}")))
    }
}

fn command_display(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(any(windows, test))]
#[derive(Debug)]
struct TemporaryPath {
    path: PathBuf,
    armed: bool,
}

#[cfg(any(windows, test))]
impl TemporaryPath {
    fn new(directory: &Path, label: &str) -> Self {
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: directory.join(format!(".coverage-gate-{}-{sequence}-{label}", std::process::id())),
            armed: true,
        }
    }

    fn write(directory: &Path, label: &str, contents: &[u8]) -> Result<Self, AppError> {
        let temporary = Self::new(directory, label);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary.path())
            .into_app_err(format!("failed to create temporary file `{}`", temporary.path().display()))?;
        file.write_all(contents)
            .into_app_err(format!("failed to write temporary file `{}`", temporary.path().display()))?;
        file.sync_all()
            .into_app_err(format!("failed to flush temporary file `{}`", temporary.path().display()))?;
        Ok(temporary)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    #[mutants::skip] // Returning Ok drops `self`, whose Drop performs the same successful removal.
    fn cleanup(mut self) -> Result<(), AppError> {
        let cleanup = fs::remove_file(&self.path).into_app_err(format!("failed to remove temporary file `{}`", self.path.display()));
        self.armed = false;
        cleanup
    }
}

#[cfg(any(windows, test))]
impl Drop for TemporaryPath {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug)]
struct TemporaryDirectory {
    path: PathBuf,
    armed: bool,
}

impl TemporaryDirectory {
    fn create(parent: &Path) -> Result<Self, AppError> {
        fs::create_dir_all(parent).into_app_err(format!("failed to create coverage target parent `{}`", parent.display()))?;
        Self::allocate(
            parent,
            || TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            |path| fs::create_dir(path),
        )
    }

    fn allocate(
        parent: &Path,
        mut next_sequence: impl FnMut() -> u64,
        mut create_dir: impl FnMut(&Path) -> io::Result<()>,
    ) -> Result<Self, AppError> {
        for _ in 0..100 {
            let sequence = next_sequence();
            let path = parent.join(format!("run-{}-{sequence}", std::process::id()));
            match create_dir(&path) {
                Ok(()) => return Ok(Self { path, armed: true }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).into_app_err(format!("failed to create isolated coverage target `{}`", path.display()));
                }
            }
        }
        Err(AppError::new(format!(
            "could not allocate a unique coverage target beneath `{}`",
            parent.display()
        )))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(mut self) -> Result<(), AppError> {
        let cleanup =
            remove_dir_if_present(&self.path).into_app_err(format!("failed to remove isolated coverage target `{}`", self.path.display()));
        self.armed = false;
        cleanup
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn remove_dir_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::cell::Cell;

    use tempfile::tempdir;

    use super::*;

    fn member(name: &str, version: &str) -> WorkspaceMember {
        WorkspaceMember {
            name: name.to_owned(),
            version: version.to_owned(),
        }
    }

    fn app_error(message: &'static str) -> AppError {
        AppError::new(message)
    }

    #[test]
    fn evaluation_error_remains_primary_when_cleanup_also_fails() {
        let finalized = combine_evaluation_and_cleanup(Err(app_error("evaluation failed")), Err(app_error("cleanup failed")));
        let error = finalized.result.expect_err("evaluation must remain an error").to_string();
        assert!(error.contains("evaluation failed"), "{error}");
        assert!(error.contains("scratch cleanup also failed: cleanup failed"), "{error}");
        assert!(finalized.cleanup_warning.is_none());
    }

    #[test]
    fn evaluation_error_is_unchanged_when_cleanup_succeeds() {
        let finalized = combine_evaluation_and_cleanup(Err(app_error("evaluation failed")), Ok(()));
        assert_eq!(
            finalized.result.expect_err("evaluation must remain an error").to_string(),
            "evaluation failed"
        );
        assert!(finalized.cleanup_warning.is_none());
    }

    #[test]
    fn policy_failure_survives_cleanup_failure_with_warning() {
        let finalized = combine_evaluation_and_cleanup(Ok(ExitCode::from(1)), Err(app_error("cleanup failed")));
        assert_eq!(
            finalized
                .cleanup_warning
                .as_ref()
                .expect("cleanup failure becomes a warning")
                .to_string(),
            "cleanup failed"
        );
        assert_eq!(finalized.complete().expect("policy result remains available"), ExitCode::from(1));
    }

    #[test]
    fn successful_evaluation_becomes_error_when_cleanup_fails() {
        let finalized = combine_evaluation_and_cleanup(Ok(ExitCode::SUCCESS), Err(app_error("cleanup failed")));
        let error = finalized.result.expect_err("cleanup failure prevents complete success").to_string();
        assert!(error.contains("cleanup failed"), "{error}");
        assert!(error.contains("coverage evaluation passed, but scratch cleanup failed"), "{error}");
        assert!(finalized.cleanup_warning.is_none());
    }

    #[test]
    fn successful_cleanup_preserves_any_evaluation_exit_code() {
        for code in [ExitCode::SUCCESS, ExitCode::from(1), ExitCode::from(2)] {
            let finalized = combine_evaluation_and_cleanup(Ok(code), Ok(()));
            assert_eq!(finalized.result.expect("exit code must be preserved"), code);
            assert!(finalized.cleanup_warning.is_none());
        }
    }

    #[test]
    fn default_configurations_include_both_supported_modes() {
        assert_eq!(
            normalized_configurations(&[]),
            [FeatureConfiguration::AllFeatures, FeatureConfiguration::NoDefaultFeatures]
        );
        assert_eq!(FeatureConfiguration::NoDefaultFeatures.artifact_name(), "no-default");
    }

    #[test]
    fn no_coverage_target_matching_is_lazy_and_exact() {
        assert!(
            configured_no_coverage_target_with(&[], None, || panic!("empty configuration must not resolve a target"))
                .expect("empty configuration")
                .is_none()
        );

        let configured = vec!["aarch64-pc-windows-msvc".to_owned()];
        let matched = configured_no_coverage_target_with(&configured, Some("aarch64-pc-windows-msvc"), || {
            panic!("an explicit target must not resolve the host")
        })
        .expect("explicit match");
        assert_eq!(matched.as_deref(), Some("aarch64-pc-windows-msvc"));

        let unmatched = configured_no_coverage_target_with(&configured, Some("x86_64-pc-windows-msvc"), || {
            panic!("an explicit target must not resolve the host")
        })
        .expect("explicit mismatch");
        assert!(unmatched.is_none());

        let calls = Cell::new(0);
        let host_match = configured_no_coverage_target_with(&configured, None, || {
            calls.set(calls.get() + 1);
            Ok("aarch64-pc-windows-msvc".to_owned())
        })
        .expect("host match");
        assert_eq!(host_match.as_deref(), Some("aarch64-pc-windows-msvc"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn tool_version_parsers_accept_only_the_documented_shapes() {
        assert_eq!(
            cargo_release("cargo 1.97.0-nightly\nrelease: 1.97.0-nightly\n"),
            Some("1.97.0-nightly")
        );
        assert_eq!(cargo_release("cargo 1.95.0\n"), None);
        assert_eq!(
            rustc_release("rustc 1.97.0-nightly\nrelease: 1.97.0-nightly\n"),
            Some("1.97.0-nightly")
        );
        assert_eq!(
            rustc_host("rustc 1.97.0-nightly\nhost: aarch64-pc-windows-msvc\n"),
            Some("aarch64-pc-windows-msvc")
        );
        assert_eq!(
            cargo_llvm_cov_version("cargo-llvm-cov 0.9.0\n").expect("valid version"),
            Version::new(0, 9, 0)
        );
        let minimum = Version::new(0, 9, 0);
        assert!(!cargo_llvm_cov_is_supported(&Version::new(0, 8, 7), &minimum));
        assert!(cargo_llvm_cov_is_supported(&Version::new(0, 9, 0), &minimum));
        cargo_llvm_cov_version("cargo llvm-cov 0.9.0\n").expect_err("missing package version prefix must fail");
        cargo_llvm_cov_version("cargo-llvm-cov development\n").expect_err("non-semver version must fail");
    }

    #[test]
    fn requested_configurations_are_deduplicated() {
        assert_eq!(
            normalized_configurations(&[
                FeatureConfiguration::NoDefaultFeatures,
                FeatureConfiguration::AllFeatures,
                FeatureConfiguration::NoDefaultFeatures,
            ]),
            [FeatureConfiguration::AllFeatures, FeatureConfiguration::NoDefaultFeatures]
        );
    }

    #[test]
    fn package_selector_matches_names_versions_and_globs() {
        let alpha = member("alpha", "1.2.3");
        assert!(selector_matches("alpha", &alpha));
        assert!(selector_matches("alpha@1.2.3", &alpha));
        assert!(selector_matches("a?pha", &alpha));
        assert!(selector_matches("alpha*", &alpha));
        assert!(!selector_matches("alpha@1.2.4", &alpha));
        assert!(!selector_matches("beta*", &alpha));
    }

    #[test]
    fn gated_names_are_emitted_only_for_explicit_selection() {
        let members = vec![member("alpha", "1.2.3"), member("beta", "2.0.0")];
        let explicit = Selection {
            explicit: true,
            members: members.clone(),
        };
        let implicit = Selection { explicit: false, members };

        assert_eq!(explicit.gated_names(), ["alpha", "beta"]);
        assert!(implicit.gated_names().is_empty());
    }

    #[test]
    fn glob_matching_covers_empty_and_repeated_star_branches() {
        assert!(glob_matches("*", ""));
        assert!(glob_matches("a**b", "axyzb"));
        assert!(!glob_matches("a?", "a"));
        assert!(!glob_matches("a*b", "ac"));
    }

    #[test]
    fn command_too_long_parser_preserves_upstream_export_arguments() {
        let stderr = concat!(
            "error: failed to generate report: could not execute process `",
            "\"C:\\Program Files\\Rust\\llvm-cov.exe\" export -format=lcov ",
            "-object \"target\\object one.exe\" ",
            "-ignore-filename-regex \"UPSTREAM_DEFAULTS\"",
            "` (never executed): The filename or extension is too long. (os error 206)"
        );
        let arguments = command_too_long_response_arguments(stderr).expect("Windows error 206 export");
        assert!(arguments.starts_with("-format=lcov"));
        assert!(arguments.contains("-object \"target\\object one.exe\""));
        assert!(arguments.contains("-ignore-filename-regex \"UPSTREAM_DEFAULTS\""));
        assert!(command_too_long_response_arguments("unrelated error").is_none());
        assert!(command_too_long_response_arguments("could not execute process `llvm-cov show` (never executed) (os error 206)").is_none());
    }

    #[test]
    fn no_coverage_data_detection_requires_the_complete_llvm_diagnostic() {
        let diagnostic = "error: failed to load coverage: 'empty': no coverage data found\n\
                          error: could not load coverage information\n";
        assert!(is_no_coverage_data(diagnostic));
        assert!(!is_no_coverage_data("error: no coverage data found"));
        assert!(!is_no_coverage_data("error: could not load coverage information"));
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn temporary_response_file_writes_and_cleans_up() {
        let tmp = tempdir().expect("tempdir");
        let temporary = TemporaryPath::write(tmp.path(), "response", b"complete bytes").expect("write temporary response");
        let path = temporary.path().to_path_buf();

        assert_eq!(fs::read(&path).expect("read temporary response"), b"complete bytes");
        temporary.cleanup().expect("clean temporary response");
        assert!(!path.exists());
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn temporary_response_drop_removes_the_file() {
        let tmp = tempdir().expect("tempdir");
        let path = {
            let temporary = TemporaryPath::write(tmp.path(), "response", b"temporary").expect("write temporary response");
            temporary.path().to_path_buf()
        };

        assert!(!path.exists());
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary directories, which Miri isolation does not support")]
    fn isolated_coverage_targets_are_unique_and_cleaned() {
        let tmp = tempdir().expect("tempdir");
        let first = TemporaryDirectory::create(tmp.path()).expect("first target");
        let second = TemporaryDirectory::create(tmp.path()).expect("second target");
        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();
        assert_ne!(first_path, second_path);
        assert!(first_path.is_dir());
        assert!(second_path.is_dir());

        first.cleanup().expect("clean first target");
        drop(second);

        assert!(!first_path.exists());
        assert!(!second_path.exists());
    }

    #[test]
    fn isolated_target_allocation_retries_collisions_and_reports_failures() {
        let parent = Path::new("coverage-target-parent");
        let mut sequence = 0_u64;
        let mut allocated = TemporaryDirectory::allocate(
            parent,
            || {
                let current = sequence;
                sequence += 1;
                current
            },
            |path| {
                if path.ends_with(format!("run-{}-0", std::process::id())) {
                    Err(io::Error::new(io::ErrorKind::AlreadyExists, "collision"))
                } else {
                    Ok(())
                }
            },
        )
        .expect("allocation retries a collision");
        assert!(allocated.path().ends_with(format!("run-{}-1", std::process::id())));
        allocated.armed = false;
        drop(allocated);

        let create_attempts = Cell::new(0);
        let error = TemporaryDirectory::allocate(
            parent,
            || 7,
            |_| {
                create_attempts.set(create_attempts.get() + 1);
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            },
        )
        .expect_err("non-collision creation failures must surface");
        assert_eq!(create_attempts.get(), 1);
        assert!(error.to_string().contains("failed to create isolated coverage target"));

        TemporaryDirectory::allocate(parent, || 9, |_| Err(io::Error::new(io::ErrorKind::AlreadyExists, "collision")))
            .expect_err("one hundred collisions must exhaust allocation");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary directories, which Miri isolation does not support")]
    fn isolated_target_cleanup_accepts_missing_and_reports_non_directories() {
        let tmp = tempdir().expect("tempdir");
        let missing = TemporaryDirectory {
            path: tmp.path().join("missing"),
            armed: true,
        };
        missing.cleanup().expect("missing target is already clean");

        let file = tmp.path().join("file");
        fs::write(&file, b"not a directory").expect("write file");
        let invalid = TemporaryDirectory { path: file, armed: true };
        invalid.cleanup().expect_err("a file cannot be removed as a target directory");
    }

    #[test]
    fn absolute_paths_are_preserved() {
        #[cfg(windows)]
        let absolute = PathBuf::from(r"C:\coverage");
        #[cfg(not(windows))]
        let absolute = PathBuf::from("/coverage");

        assert_eq!(
            absolute_path_with(&absolute, || panic!("absolute paths must not query the current directory")).expect("absolute path"),
            absolute
        );
    }

    #[test]
    fn relative_paths_are_anchored_to_the_invocation_directory() {
        #[cfg(windows)]
        let current = PathBuf::from(r"C:\workspace");
        #[cfg(not(windows))]
        let current = PathBuf::from("/workspace");

        assert_eq!(
            absolute_path_with(Path::new("coverage"), || Ok(current.clone())).expect("relative path"),
            current.join("coverage")
        );
    }
}
