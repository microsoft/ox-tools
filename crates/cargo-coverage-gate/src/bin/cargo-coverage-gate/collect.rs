// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Portable coverage collection for `cargo coverage-gate run`.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fs, io};

use cargo_metadata::{Metadata, MetadataCommand};
use ohno::{AppError, EnrichableExt as _, IntoAppError};
use semver::Version;

use crate::cli::{CollectionArgs, CoverageGateArgs, FeatureConfiguration};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MIN_CARGO_LLVM_COV_VERSION: &str = "0.9.0";
const METADATA_LOAD_CONTEXT: &str = "failed to load cargo workspace metadata";
const NEXTEST_DESCRIPTION: &str = "cargo llvm-cov nextest";
const WINDOWS_DIAGNOSTIC_UTF8_CONTEXT: &str = "cargo-llvm-cov's Windows command-too-long diagnostic was not UTF-8";
#[cfg(any(windows, test))]
const RUSTC_TARGET_LIBDIR_PARENT_ERROR: &str = "rustc target-libdir output had no parent directory";

pub(crate) fn run(args: &CoverageGateArgs, collection: &CollectionArgs) -> Result<ExitCode, AppError> {
    if !args.lcov.is_empty() {
        return Err(AppError::new(
            "`--lcov` cannot be used with `cargo coverage-gate run`; collected LCOV paths are selected by `--coverage-dir`",
        ));
    }

    let tools = ToolPrograms::from_env();
    let (workspace, selection, collection) = prepare_collection(args, collection, &tools, WorkspaceInfo::load, resolved_collection)?;
    let configurations = normalized_configurations(&collection.configurations);
    let effective_target = EffectiveTarget::resolve(&workspace, args.target.as_deref(), &tools)?;

    if configured_no_coverage_target(&collection.no_coverage_targets, &effective_target.triple) {
        let result = format!(
            "target `{}` is configured for no coverage; tests passed without coverage collection or gating",
            effective_target.triple
        );
        finish_no_coverage_run(
            args,
            &result,
            || {
                run_plain_configurations(
                    &workspace,
                    &selection,
                    &collection,
                    &configurations,
                    &effective_target.triple,
                    &tools,
                    args.quiet,
                )
            },
            crate::run::write_no_gate_summary,
        )?;
        eprintln!("coverage-gate: {result}");
        return Ok(ExitCode::SUCCESS);
    }

    validate_instrumentation_tools(&workspace, &tools, effective_target.rustc_version.as_deref())?;
    let coverage_scratch = prepare_coverage_directories(
        &collection.coverage_dir,
        &workspace,
        |path| fs::create_dir_all(path),
        TemporaryDirectory::create,
    )?;
    let coverage_target_root = coverage_target_root(coverage_scratch.path());
    let execution = CollectionExecution {
        workspace: &workspace,
        selection: &selection,
        args: &collection,
        #[cfg(windows)]
        scratch_dir: coverage_scratch.path(),
        coverage_target_root: &coverage_target_root,
        target: &effective_target.triple,
        tools: &tools,
        quiet: args.quiet,
    };
    let mut lcov_paths = Vec::with_capacity(configurations.len());
    for configuration in configurations {
        lcov_paths.push(collect_configuration(&execution, configuration)?);
    }

    let evaluation = crate::run::evaluate_paths(args, &lcov_paths, &selection.gated_names(), Some(&effective_target.triple));
    combine_evaluation_and_cleanup(evaluation, coverage_scratch.cleanup()).complete()
}

fn prepare_collection(
    args: &CoverageGateArgs,
    collection: &CollectionArgs,
    tools: &ToolPrograms,
    load_workspace: impl FnOnce(&ToolPrograms) -> Result<WorkspaceInfo, AppError>,
    resolve_collection: impl FnOnce(&CollectionArgs) -> Result<CollectionArgs, AppError>,
) -> Result<(WorkspaceInfo, Selection, CollectionArgs), AppError> {
    let workspace = load_workspace(tools)?;
    let selection = Selection::resolve(&workspace, &args.packages)?;
    let collection = resolve_collection(collection)?;
    Ok((workspace, selection, collection))
}

fn finish_no_coverage_run(
    args: &CoverageGateArgs,
    result: &str,
    run_plain: impl FnOnce() -> Result<(), AppError>,
    write_summary: impl FnOnce(&CoverageGateArgs, &str) -> Result<(), AppError>,
) -> Result<(), AppError> {
    run_plain()?;
    write_summary(args, result)
}

fn prepare_coverage_directories(
    coverage_dir: &Path,
    workspace: &WorkspaceInfo,
    create_coverage_dir: impl FnOnce(&Path) -> io::Result<()>,
    create_scratch: impl FnOnce(&Path) -> Result<TemporaryDirectory, AppError>,
) -> Result<TemporaryDirectory, AppError> {
    create_coverage_dir(coverage_dir).into_app_err(format!("failed to create coverage directory `{}`", coverage_dir.display()))?;
    create_scratch(&coverage_scratch_parent(workspace))
}

fn resolved_collection(collection: &CollectionArgs) -> Result<CollectionArgs, AppError> {
    resolved_collection_with(collection, absolute_path)
}

fn resolved_collection_with(
    collection: &CollectionArgs,
    resolve: impl FnOnce(&Path) -> Result<PathBuf, AppError>,
) -> Result<CollectionArgs, AppError> {
    let mut resolved = collection.clone();
    resolved.coverage_dir = resolve(&resolved.coverage_dir)?;
    Ok(resolved)
}

fn coverage_scratch_parent(workspace: &WorkspaceInfo) -> PathBuf {
    workspace.target_dir.join("coverage-gate")
}

fn coverage_target_root(scratch: &Path) -> PathBuf {
    scratch.join("cargo-target")
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
    coverage_target_root: &'a Path,
    target: &'a str,
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
        Self::from_env_with(|name| env::var_os(name))
    }

    fn from_env_with(mut var_os: impl FnMut(&str) -> Option<OsString>) -> Self {
        Self {
            cargo: var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")),
            rustc: var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc")),
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

#[derive(Debug)]
struct EffectiveTarget {
    triple: String,
    rustc_version: Option<String>,
}

impl EffectiveTarget {
    fn resolve(workspace: &WorkspaceInfo, explicit: Option<&str>, tools: &ToolPrograms) -> Result<Self, AppError> {
        Self::resolve_with_reader(workspace, explicit, tools, read_rustc_version)
    }

    fn resolve_with_reader(
        workspace: &WorkspaceInfo,
        explicit: Option<&str>,
        tools: &ToolPrograms,
        read_version: impl FnOnce(&WorkspaceInfo, &ToolPrograms, &str) -> Result<String, AppError>,
    ) -> Result<Self, AppError> {
        if let Some(triple) = explicit {
            return Ok(Self {
                triple: triple.to_owned(),
                rustc_version: None,
            });
        }

        let rustc_version = read_version(workspace, tools, "rustc host-target discovery")?;
        let triple = rustc_host(&rustc_version)
            .map(str::to_owned)
            .ok_or_else(|| AppError::new("`rustc -vV` did not report a host target"))?;
        Ok(Self {
            triple,
            rustc_version: Some(rustc_version),
        })
    }
}

impl WorkspaceInfo {
    fn load(tools: &ToolPrograms) -> Result<Self, AppError> {
        let mut command = MetadataCommand::new();
        command.no_deps().cargo_path(tools.cargo());
        let metadata = command.exec().into_app_err(METADATA_LOAD_CONTEXT)?;
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
            let diagnostic = crate::package_glob::diagnostic(selector);
            let pattern = crate::package_glob::parse(selector)
                .map_err(|error| AppError::new(format!("invalid `--package` selector `{diagnostic}`: {error}")))?;
            let matches = workspace
                .members
                .iter()
                .filter(|member| selector_matches(selector, &pattern, member))
                .cloned()
                .collect::<Vec<_>>();
            if matches.is_empty() {
                return Err(AppError::new(format!(
                    "`--package` selector `{diagnostic}` did not match any workspace member"
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

fn selector_matches(selector: &str, pattern: &glob::Pattern, member: &WorkspaceMember) -> bool {
    selector == member.spec() || pattern.matches(&member.name)
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

fn configured_no_coverage_target(configured: &[String], target: &str) -> bool {
    configured.iter().any(|candidate| candidate == target)
}

fn read_rustc_version(workspace: &WorkspaceInfo, tools: &ToolPrograms, description: &str) -> Result<String, AppError> {
    read_rustc_version_with(workspace, tools, description, read_stdout)
}

fn read_rustc_version_with(
    workspace: &WorkspaceInfo,
    tools: &ToolPrograms,
    description: &str,
    mut read: impl FnMut(&mut Command, &str) -> Result<String, AppError>,
) -> Result<String, AppError> {
    let mut rustc_version = Command::new(tools.rustc());
    rustc_version.arg("-vV").current_dir(&workspace.root);
    read(&mut rustc_version, description)
}

fn validate_instrumentation_tools(
    workspace: &WorkspaceInfo,
    tools: &ToolPrograms,
    resolved_rustc_version: Option<&str>,
) -> Result<(), AppError> {
    validate_instrumentation_tools_with(workspace, tools, resolved_rustc_version, read_stdout)
}

fn validate_instrumentation_tools_with(
    workspace: &WorkspaceInfo,
    tools: &ToolPrograms,
    resolved_rustc_version: Option<&str>,
    mut read: impl FnMut(&mut Command, &str) -> Result<String, AppError>,
) -> Result<(), AppError> {
    let mut cargo_version = cargo_command(workspace, tools);
    cargo_version.args(["--version", "--verbose"]);
    let cargo_version = read(&mut cargo_version, "cargo toolchain validation")?;
    let cargo_release =
        cargo_release(&cargo_version).ok_or_else(|| AppError::new("`cargo --version --verbose` did not report a release"))?;
    if !cargo_release.contains("-nightly") {
        return Err(AppError::new(format!(
            "`cargo coverage-gate run` requires nightly Cargo for `cfg(coverage_nightly)`, but the effective Cargo release is `{cargo_release}`"
        )));
    }

    let rustc_version = if let Some(output) = resolved_rustc_version {
        output.to_owned()
    } else {
        read_rustc_version_with(workspace, tools, "rustc toolchain validation", &mut read)?
    };
    let rustc_release = rustc_release(&rustc_version).ok_or_else(|| AppError::new("`rustc -vV` did not report a release"))?;
    if !rustc_release.contains("-nightly") {
        return Err(AppError::new(format!(
            "`cargo coverage-gate run` requires nightly rustc for `cfg(coverage_nightly)`, but the effective rustc release is `{rustc_release}`"
        )));
    }

    let mut llvm_cov_version = cargo_command(workspace, tools);
    llvm_cov_version.args(["llvm-cov", "--version"]);
    let llvm_cov_version = read(&mut llvm_cov_version, "cargo-llvm-cov version validation")?;
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
    read_stdout_with(command, description, Child::wait_with_output)
}

fn read_stdout_with(
    command: &mut Command,
    description: &str,
    wait_with_output: impl FnOnce(Child) -> io::Result<Output>,
) -> Result<String, AppError> {
    let display = command_display(command);
    command.stdout(Stdio::piped()).stderr(Stdio::inherit());
    let child = command.spawn().into_app_err(format!("failed to execute `{display}`"))?;
    let output = wait_with_output(child).into_app_err(format!("failed to wait for `{display}`"))?;
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
    target: &str,
    tools: &ToolPrograms,
    quiet: bool,
) -> Result<(), AppError> {
    for &configuration in configurations {
        let mut command = plain_configuration_command(workspace, selection, args, configuration, target, tools);
        if quiet {
            command.stdout(Stdio::null());
        }
        run_status(&mut command, "cargo nextest")?;
    }

    Ok(())
}

fn plain_configuration_command(
    workspace: &WorkspaceInfo,
    selection: &Selection,
    args: &CollectionArgs,
    configuration: FeatureConfiguration,
    target: &str,
    tools: &ToolPrograms,
) -> Command {
    let mut command = cargo_command(workspace, tools);
    command.args(["nextest", "run"]);
    append_package_selection(&mut command, selection);
    append_nextest_options(&mut command, args, configuration, target);
    command.arg("--no-tests=pass");
    command
}

fn collect_configuration(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration) -> Result<PathBuf, AppError> {
    let lcov_path = execution
        .args
        .coverage_dir
        .join(format!("lcov-{}.info", configuration.artifact_name()));

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
        .env("CARGO_TARGET_DIR", coverage_target_dir)
        .env("CARGO_LLVM_COV_TARGET_DIR", coverage_target_dir)
        .env("CARGO_LLVM_COV_BUILD_DIR", coverage_target_dir);
    command
}

fn configuration_target_dir(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration) -> PathBuf {
    execution.coverage_target_root.join(configuration.artifact_name())
}

fn run_nextest(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration) -> Result<(), AppError> {
    let mut command = nextest_command(execution, configuration);
    if execution.quiet {
        command.stdout(Stdio::null());
    }
    run_status(&mut command, NEXTEST_DESCRIPTION)
}

fn nextest_command(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration) -> Command {
    let target_dir = configuration_target_dir(execution, configuration);
    let mut command = coverage_command(execution.workspace, &target_dir, execution.tools);
    command.args(["llvm-cov", "nextest", "--no-report"]);
    append_package_selection(&mut command, execution.selection);
    append_nextest_options(&mut command, execution.args, configuration, execution.target);
    command.arg("--no-tests=pass");
    command
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

fn append_nextest_options(command: &mut Command, args: &CollectionArgs, configuration: FeatureConfiguration, target: &str) {
    command.arg(configuration.cargo_flag()).arg("--locked").arg("--target").arg(target);
    if let Some(jobs) = args.jobs {
        command.arg("--jobs").arg(jobs.get().to_string());
        command.arg("--build-jobs").arg(jobs.get().to_string());
    }
}

#[cfg(any(windows, test))]
fn prefixed_path_argument(prefix: &str, path: &Path) -> OsString {
    let mut argument = OsString::from(prefix);
    argument.push(path.as_os_str());
    argument
}

#[derive(Debug)]
/// The classified next step after inspecting `cargo llvm-cov report` output.
enum ReportAction {
    Complete,
    WindowsFallback(Vec<String>),
}

fn run_report(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration, lcov_path: &Path) -> Result<(), AppError> {
    let mut command = report_command(execution, configuration, lcov_path);
    let display = command_display(&command);
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .into_app_err(format!("failed to execute `{display}`"))?;
    match handle_report_output_with(
        &output,
        execution.quiet,
        &display,
        lcov_path,
        cfg!(windows),
        forward_output,
        |output, quiet| forward_stdout_to(output, quiet, &mut io::stdout()),
        |path| fs::write(path, []).into_app_err(format!("failed to write empty LCOV file `{}`", path.display())),
    )? {
        ReportAction::Complete => Ok(()),
        ReportAction::WindowsFallback(arguments) => {
            #[cfg(windows)]
            {
                eprintln!(
                    "coverage-gate: cargo-llvm-cov could not launch llvm-cov directly; retrying its export through an LLVM response file"
                );
                run_windows_report_fallback(execution, configuration, lcov_path, &arguments)
            }
            #[cfg(not(windows))]
            {
                let _ = (execution, configuration, lcov_path, arguments);
                Err(AppError::new("Windows response-file fallback was requested on a non-Windows host"))
            }
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the injected output boundaries keep report classification deterministic and independently testable"
)]
fn handle_report_output_with(
    output: &Output,
    quiet: bool,
    display: &str,
    lcov_path: &Path,
    allow_windows_fallback: bool,
    mut forward: impl FnMut(&Output, bool) -> Result<(), AppError>,
    mut forward_stdout: impl FnMut(&Output, bool) -> Result<(), AppError>,
    write_empty: impl FnOnce(&Path) -> Result<(), AppError>,
) -> Result<ReportAction, AppError> {
    if output.status.success() {
        forward(output, quiet)?;
        return Ok(ReportAction::Complete);
    }

    if allow_windows_fallback && is_windows_command_too_long(&output.stderr) {
        forward(output, quiet)?;
        let stderr = std::str::from_utf8(&output.stderr).into_app_err(WINDOWS_DIAGNOSTIC_UTF8_CONTEXT)?;
        let arguments = command_too_long_response_arguments(stderr)?;
        return Ok(ReportAction::WindowsFallback(arguments));
    }

    if is_no_coverage_data(&String::from_utf8_lossy(&output.stderr)) {
        forward_stdout(output, quiet)?;
        write_empty(lcov_path)?;
        eprintln!("coverage-gate: cargo-llvm-cov found no coverable objects; evaluating an empty LCOV report");
        return Ok(ReportAction::Complete);
    }

    forward(output, quiet)?;
    Err(AppError::new(format!(
        "cargo llvm-cov report failed: `{display}` exited with {}",
        output.status
    )))
}

fn report_command(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration, lcov_path: &Path) -> Command {
    let target_dir = configuration_target_dir(execution, configuration);
    let mut command = coverage_command(execution.workspace, &target_dir, execution.tools);
    #[cfg(windows)]
    command.env_remove("MSYSTEM");
    command.args(["llvm-cov", "report", "--lcov", "--output-path"]).arg(lcov_path);
    append_package_selection(&mut command, execution.selection);
    command.arg("--target").arg(execution.target);
    command
}

fn forward_stdout_to(output: &Output, quiet: bool, stdout: &mut dyn io::Write) -> Result<(), AppError> {
    if !quiet {
        stdout
            .write_all(&output.stdout)
            .into_app_err("failed to forward cargo-llvm-cov stdout")?;
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn stdout_forwarder(stdout: &mut dyn io::Write) -> impl FnMut(&Output, bool) -> Result<(), AppError> + '_ {
    move |output, quiet| forward_stdout_to(output, quiet, stdout)
}

fn forward_output(output: &Output, quiet: bool) -> Result<(), AppError> {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    forward_output_to(output, quiet, &mut stdout, &mut stderr)
}

fn forward_output_to(output: &Output, quiet: bool, stdout: &mut dyn io::Write, stderr: &mut dyn io::Write) -> Result<(), AppError> {
    forward_stdout_to(output, quiet, stdout)?;
    stderr
        .write_all(&output.stderr)
        .into_app_err("failed to forward cargo-llvm-cov stderr")
}

fn is_no_coverage_data(stderr: &str) -> bool {
    stderr.contains("no coverage data found") && stderr.contains("could not load coverage information")
}

fn is_windows_command_too_long(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr).contains("(os error 206)")
}

fn command_too_long_response_arguments(stderr: &str) -> Result<Vec<String>, AppError> {
    if !stderr.contains("(os error 206)") {
        return Err(AppError::new("cargo-llvm-cov did not report Windows error 206"));
    }
    let after_prefix = stderr
        .split_once("could not execute process `")
        .ok_or_else(|| {
            AppError::new("cargo-llvm-cov's Windows command-too-long diagnostic did not contain the expected process-error prefix")
        })?
        .1;
    let command = after_prefix
        .rsplit_once("` (never executed)")
        .ok_or_else(|| {
            AppError::new("cargo-llvm-cov's Windows command-too-long diagnostic did not contain the expected process-error suffix")
        })?
        .0;
    let argv = parse_windows_command_line(command)?;
    if argv.len() < 3 || argv[0].is_empty() || argv[1] != "export" {
        return Err(AppError::new(
            "cargo-llvm-cov's Windows command-too-long diagnostic did not contain an llvm-cov export command",
        ));
    }
    Ok(argv.into_iter().skip(2).collect())
}

fn parse_windows_command_line(command: &str) -> Result<Vec<String>, AppError> {
    if command.contains('\0') {
        return Err(AppError::new(
            "cargo-llvm-cov's Windows command-too-long diagnostic contained a null byte",
        ));
    }

    let mut chars = command.chars().peekable();
    let mut arguments = Vec::new();
    loop {
        while
        // #[gamma::skip(cond.negate, tag = "outofmemory", reason = "preventing the whitespace scan from terminating exhausts memory")]
        chars.peek().is_some_and(char::is_ascii_whitespace) {
            // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "the parser must advance past leading whitespace")]
            chars.next();
        }
        if
        // #[gamma::skip(cond.always_false, tag = "outofmemory", reason = "ignoring end of input leaves the outer parser loop running")]
        chars.peek().is_none() {
            // #[gamma::skip(loop.delete_break, tag = "outofmemory", reason = "the outer parser loop must terminate at end of input")]
            // #[gamma::skip(loop.break_to_continue, tag = "timeout", reason = "continuing at end of input leaves the outer parser loop running")]
            break;
        }

        let mut argument = String::new();
        let mut quoted = false;
        loop {
            let mut backslashes = String::new();
            while matches!(chars.peek(), Some('\\')) {
                // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "the parser must consume each counted backslash")]
                backslashes.push(chars.next().expect("peeked backslash remains available"));
            }

            if
            // #[gamma::skip(cond.always_true, cond.negate, tag = "timeout", reason = "misclassifying every backslash run as a quote prevents the parser from making progress")]
            matches!(chars.peek(), Some('"')) {
                // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "the parser must consume the opening or escaped quote")]
                chars.next();
                let mut pairs = backslashes.as_bytes().chunks_exact(2);
                argument.extend(pairs.by_ref().map(|_| '\\'));
                if pairs.remainder().is_empty() {
                    quoted = !quoted;
                } else {
                    argument.push('"');
                }
                continue;
            }

            argument.push_str(&backslashes);
            let Some(&character) = chars.peek() else {
                // #[gamma::skip(loop.break_to_continue, tag = "timeout", reason = "continuing after reaching end of input leaves the quoted parser loop running")]
                break;
            };
            if quoted {
                // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "the quoted parser must consume each character")]
                argument.push(chars.next().expect("peeked quoted character remains available"));
                continue;
            }
            if
            // #[gamma::skip(cond.always_true, cond.negate, tag = "outofmemory", reason = "misclassifying unquoted characters prevents the parser from advancing")]
            character.is_ascii_whitespace() {
                // #[gamma::skip(loop.break_to_continue, tag = "timeout", reason = "the argument parser must stop at unquoted whitespace")]
                break;
            }
            // #[gamma::skip(stmt.delete_call, tag = "timeout", reason = "the unquoted parser must consume each command character")]
            argument.push(chars.next().expect("peeked command character remains available"));
        }
        if quoted {
            return Err(AppError::new(
                "cargo-llvm-cov's Windows command-too-long diagnostic contained an unterminated quoted argument",
            ));
        }
        arguments.push(argument);
    }

    Ok(arguments)
}

#[cfg(any(windows, test))]
fn windows_response_contents(arguments: &[String]) -> Result<Vec<u8>, AppError> {
    if arguments.is_empty() {
        return Err(AppError::new(
            "cargo-llvm-cov's Windows command-too-long diagnostic contained no export arguments",
        ));
    }

    let mut response = String::new();
    for argument in arguments {
        if argument.contains('\0') {
            return Err(AppError::new(
                "cargo-llvm-cov's Windows command-too-long diagnostic contained a null byte",
            ));
        }
        quote_windows_argument(argument, &mut response);
        response.push('\n');
    }
    Ok(response.into_bytes())
}

#[cfg(any(windows, test))]
fn quote_windows_argument(argument: &str, output: &mut String) {
    output.push('"');
    let mut backslashes = String::new();
    for character in argument.chars() {
        if character == '\\' {
            backslashes.push(character);
            continue;
        }
        output.push_str(&backslashes);
        if character == '"' {
            output.push_str(&backslashes);
            output.push('\\');
        }
        backslashes.clear();
        output.push(character);
    }
    output.push_str(&backslashes);
    output.push_str(&backslashes);
    output.push('"');
}

#[cfg(windows)]
// The spawned-binary integration test covers this fallback, but the outer
// coverage report cannot include that child binary's coverage object.
#[mutants::skip] // Thin Windows adapter over the exhaustively tested injected fallback seam.
#[cfg_attr(coverage_nightly, coverage(off))]
fn run_windows_report_fallback(
    execution: &CollectionExecution<'_>,
    configuration: FeatureConfiguration,
    lcov_path: &Path,
    arguments: &[String],
) -> Result<(), AppError> {
    let mut stdout = io::stdout();
    run_windows_report_fallback_with(
        execution,
        execution.scratch_dir,
        configuration,
        lcov_path,
        arguments,
        windows_response_contents,
        TemporaryPath::write,
        TemporaryPath::create,
        discover_llvm_cov,
        execute_windows_export,
        forward_output,
        stdout_forwarder(&mut stdout),
        |path| fs::write(path, []).into_app_err(format!("failed to write empty staged LCOV file `{}`", path.display())),
        TemporaryPath::publish,
    )
}

#[cfg(any(windows, test))]
#[expect(
    clippy::too_many_arguments,
    reason = "the injected Windows fallback boundaries allow each filesystem and process failure to be tested independently"
)]
fn run_windows_report_fallback_with(
    execution: &CollectionExecution<'_>,
    scratch_dir: &Path,
    configuration: FeatureConfiguration,
    lcov_path: &Path,
    arguments: &[String],
    encode_response: impl FnOnce(&[String]) -> Result<Vec<u8>, AppError>,
    write_response: impl FnOnce(&Path, &str, &[u8]) -> Result<TemporaryPath, AppError>,
    create_staged: impl FnOnce(&Path, &str) -> Result<(TemporaryPath, fs::File), AppError>,
    discover_cov: impl FnOnce(&ToolPrograms) -> Result<PathBuf, AppError>,
    execute_export: impl FnOnce(PathBuf, &Path, &Path, fs::File) -> Result<(String, Output), AppError>,
    mut forward: impl FnMut(&Output, bool) -> Result<(), AppError>,
    mut forward_stdout: impl FnMut(&Output, bool) -> Result<(), AppError>,
    write_empty: impl FnOnce(&Path) -> Result<(), AppError>,
    publish: impl FnOnce(TemporaryPath, &Path) -> Result<(), AppError>,
) -> Result<(), AppError> {
    let response_contents = encode_response(arguments)?;
    let response = write_response(
        scratch_dir,
        &format!("{}-objects.rsp", configuration.artifact_name()),
        &response_contents,
    )?;
    let output_dir = lcov_path
        .parent()
        .ok_or_else(|| AppError::new(format!("LCOV path `{}` has no parent directory", lcov_path.display())))?;
    let (staged, output_file) = create_staged(output_dir, &format!("{}-fallback.info", configuration.artifact_name()))?;
    let llvm_cov = discover_cov(execution.tools)?;
    let (display, output) = execute_export(llvm_cov, &execution.workspace.root, response.path(), output_file)?;
    if output.status.success() {
        forward(&output, execution.quiet)?;
        return publish(staged, lcov_path);
    }

    if is_no_coverage_data(&String::from_utf8_lossy(&output.stderr)) {
        forward_stdout(&output, execution.quiet)?;
        write_empty(staged.path())?;
        publish(staged, lcov_path)?;
        eprintln!("coverage-gate: llvm-cov found no coverable objects during response-file retry; evaluating an empty LCOV report");
        return Ok(());
    }

    forward(&output, execution.quiet)?;
    Err(AppError::new(format!(
        "llvm-cov export response-file fallback failed: `{display}` exited with {}",
        output.status
    )))
}

#[cfg(any(windows, test))]
#[cfg_attr(coverage_nightly, coverage(off))]
fn execute_windows_export(
    llvm_cov: PathBuf,
    workspace_root: &Path,
    response: &Path,
    output_file: fs::File,
) -> Result<(String, Output), AppError> {
    let mut command = Command::new(llvm_cov);
    configure_windows_export_command(&mut command, workspace_root, response);
    command.stdout(Stdio::from(output_file));
    let display = command_display(&command);
    let output = command.output().into_app_err(format!("failed to execute `{display}`"))?;
    Ok((display, output))
}

#[cfg(any(windows, test))]
fn configure_windows_export_command(command: &mut Command, workspace_root: &Path, response: &Path) {
    command
        .current_dir(workspace_root)
        .arg("export")
        .arg(prefixed_path_argument("@", response))
        .stderr(Stdio::piped());
}

#[cfg(windows)]
// Linux mutation jobs cannot execute Windows LLVM discovery. The Windows
// integration fallback covers both the explicit override and discovered tool.
#[mutants::skip]
// Exercised through the spawned-binary fallback integration test.
#[cfg_attr(coverage_nightly, coverage(off))]
fn discover_llvm_cov(tools: &ToolPrograms) -> Result<PathBuf, AppError> {
    discover_llvm_cov_with(tools, |name| env::var_os(name), read_stdout)
}

#[cfg(any(windows, test))]
fn discover_llvm_cov_with(
    tools: &ToolPrograms,
    var_os: impl FnOnce(&str) -> Option<OsString>,
    read: impl FnOnce(&mut Command, &str) -> Result<String, AppError>,
) -> Result<PathBuf, AppError> {
    if let Some(cov) = var_os("LLVM_COV") {
        return Ok(PathBuf::from(cov));
    }

    let mut command = Command::new(tools.rustc());
    command.args(["--print", "target-libdir"]);
    let target_libdir = read(&mut command, "rustc LLVM-tool discovery")?;
    let rustlib = Path::new(target_libdir.trim())
        .parent()
        .ok_or_else(|| AppError::new(RUSTC_TARGET_LIBDIR_PARENT_ERROR))?;
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
        .map(|argument| {
            let lossy = argument.to_string_lossy();
            if lossy.is_empty() || lossy.chars().any(char::is_whitespace) {
                format!("{lossy:?}")
            } else {
                lossy.into_owned()
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

#[cfg(any(windows, test))]
#[derive(Debug)]
struct TemporaryPath {
    path: PathBuf,
    armed: bool,
}

#[cfg(any(windows, test))]
/// A temporary-file writer boundary that makes durability failures independently testable.
trait SynchronizedWrite: io::Write {
    fn sync_all(&self) -> io::Result<()>;
}

#[cfg(any(windows, test))]
impl SynchronizedWrite for fs::File {
    #[mutants::skip] // Trivial File delegation; the injected writer test double covers synchronization failures.
    fn sync_all(&self) -> io::Result<()> {
        Self::sync_all(self)
    }
}

#[cfg(any(windows, test))]
impl TemporaryPath {
    fn new(directory: &Path, label: &str) -> Self {
        let sequence = reserve_temporary_sequence(&TEMPORARY_SEQUENCE);
        Self {
            path: directory.join(format!(".coverage-gate-{}-{sequence}-{label}", std::process::id())),
            armed: true,
        }
    }

    fn write(directory: &Path, label: &str, contents: &[u8]) -> Result<Self, AppError> {
        let (temporary, mut file) = Self::create(directory, label)?;
        Self::finish_write(temporary, &mut file, contents)
    }

    fn finish_write(temporary: Self, writer: &mut impl SynchronizedWrite, contents: &[u8]) -> Result<Self, AppError> {
        write_temporary_contents(&temporary, writer, contents)?;
        Ok(temporary)
    }

    fn create(directory: &Path, label: &str) -> Result<(Self, fs::File), AppError> {
        let temporary = Self::new(directory, label);
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary.path())
            .into_app_err(format!("failed to create temporary file `{}`", temporary.path().display()))?;
        Ok((temporary, file))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn publish(self, target: &Path) -> Result<(), AppError> {
        fs::rename(&self.path, target).into_app_err(format!(
            "failed to publish staged coverage report `{}` to `{}`",
            self.path.display(),
            target.display()
        ))?;
        Ok(())
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

#[cfg(any(windows, test))]
fn write_temporary_contents(temporary: &TemporaryPath, writer: &mut impl SynchronizedWrite, contents: &[u8]) -> Result<(), AppError> {
    writer
        .write_all(contents)
        .into_app_err(format!("failed to write temporary file `{}`", temporary.path().display()))?;
    writer
        .sync_all()
        .into_app_err(format!("failed to flush temporary file `{}`", temporary.path().display()))
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
            || reserve_temporary_sequence(&TEMPORARY_SEQUENCE),
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

    fn cleanup(self) -> Result<(), AppError> {
        remove_dir_if_present(&self.path).into_app_err(format!("failed to remove isolated coverage target `{}`", self.path.display()))
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

fn reserve_temporary_sequence(sequence: &AtomicU64) -> u64 {
    sequence.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::cell::Cell;
    use std::num::NonZeroUsize;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt as _;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt as _;
    use std::process::{ExitStatus, id as process_id};

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

    fn process_output(success: bool, stdout: &[u8], stderr: &[u8]) -> Output {
        #[cfg(unix)]
        let status = ExitStatus::from_raw(if success { 0 } else { 1 << 8 });
        #[cfg(windows)]
        let status = ExitStatus::from_raw(u32::from(!success));

        Output {
            status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    fn workspace(root: &Path) -> WorkspaceInfo {
        WorkspaceInfo {
            root: root.to_path_buf(),
            target_dir: root.join("target"),
            members: vec![member("zeta", "2.0.0"), member("alpha", "1.0.0")],
        }
    }

    fn collection_args() -> CollectionArgs {
        CollectionArgs {
            configurations: Vec::new(),
            coverage_dir: PathBuf::from("coverage"),
            no_coverage_targets: Vec::new(),
            jobs: NonZeroUsize::new(3),
        }
    }

    fn coverage_args() -> CoverageGateArgs {
        CoverageGateArgs {
            lcov: Vec::new(),
            packages: Vec::new(),
            target: None,
            summary_file: None,
            quiet: false,
            command: None,
        }
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
    fn documented_tool_environment_keys_and_fallbacks_are_exact() {
        let mut queried = Vec::new();
        let tools = ToolPrograms::from_env_with(|name| {
            queried.push(name.to_owned());
            (name == "RUSTC").then(|| OsString::from("custom-rustc"))
        });
        assert_eq!(queried, ["CARGO", "RUSTC"]);
        assert_eq!(tools.cargo(), OsStr::new("cargo"));
        assert_eq!(tools.rustc(), OsStr::new("custom-rustc"));
    }

    #[test]
    fn stable_collection_error_contexts_are_exact() {
        assert_eq!(METADATA_LOAD_CONTEXT, "failed to load cargo workspace metadata");
        assert_eq!(NEXTEST_DESCRIPTION, "cargo llvm-cov nextest");
        assert_eq!(
            WINDOWS_DIAGNOSTIC_UTF8_CONTEXT,
            "cargo-llvm-cov's Windows command-too-long diagnostic was not UTF-8"
        );
        assert_eq!(
            RUSTC_TARGET_LIBDIR_PARENT_ERROR,
            "rustc target-libdir output had no parent directory"
        );
    }

    #[test]
    fn scratch_paths_use_stable_component_names() {
        let workspace = workspace(Path::new("repo"));
        assert_eq!(coverage_scratch_parent(&workspace), PathBuf::from("repo/target/coverage-gate"));
        assert_eq!(
            coverage_target_root(Path::new("repo/target/coverage-gate/run-1")),
            PathBuf::from("repo/target/coverage-gate/run-1/cargo-target")
        );
    }

    #[test]
    fn collection_resolution_replaces_the_requested_coverage_directory() {
        let collection = collection_args();
        let resolved = resolved_collection_with(&collection, |path| {
            assert_eq!(path, Path::new("coverage"));
            Ok(PathBuf::from("absolute-coverage"))
        })
        .expect("resolution succeeds");
        assert_eq!(resolved.coverage_dir, PathBuf::from("absolute-coverage"));
    }

    #[test]
    fn collection_resolution_propagates_path_errors() {
        let error = resolved_collection_with(&collection_args(), |_| Err(app_error("resolution failed")))
            .expect_err("resolution errors must propagate");
        assert_eq!(error.to_string(), "resolution failed");
    }

    #[test]
    fn collection_preparation_returns_each_dependency_error() {
        let tools = ToolPrograms {
            cargo: OsString::from("cargo"),
            rustc: OsString::from("rustc"),
        };
        let load_error = prepare_collection(
            &coverage_args(),
            &collection_args(),
            &tools,
            |_| Err(app_error("workspace failed")),
            |_| panic!("collection resolution must not run"),
        )
        .expect_err("workspace failure must propagate");
        assert_eq!(load_error.to_string(), "workspace failed");

        let mut selector_args = coverage_args();
        selector_args.packages.push("missing".to_owned());
        let selector_error = prepare_collection(
            &selector_args,
            &collection_args(),
            &tools,
            |_| Ok(workspace(Path::new("repo"))),
            |_| panic!("collection resolution must not run"),
        )
        .expect_err("selector failure must propagate");
        assert!(selector_error.to_string().contains("missing"));

        let resolution_error = prepare_collection(
            &coverage_args(),
            &collection_args(),
            &tools,
            |_| Ok(workspace(Path::new("repo"))),
            |_| Err(app_error("collection failed")),
        )
        .expect_err("collection resolution failure must propagate");
        assert_eq!(resolution_error.to_string(), "collection failed");
    }

    #[test]
    fn no_coverage_completion_returns_test_and_summary_errors() {
        let test_error = finish_no_coverage_run(
            &coverage_args(),
            "skipped",
            || Err(app_error("plain tests failed")),
            |_, _| panic!("summary must not run"),
        )
        .expect_err("plain-test failure must propagate");
        assert_eq!(test_error.to_string(), "plain tests failed");

        let summary_error = finish_no_coverage_run(&coverage_args(), "skipped", || Ok(()), |_, _| Err(app_error("summary failed")))
            .expect_err("summary failure must propagate");
        assert_eq!(summary_error.to_string(), "summary failed");
    }

    #[test]
    fn coverage_directory_preparation_returns_each_error() {
        let workspace = workspace(Path::new("repo"));
        let coverage_error = prepare_coverage_directories(
            Path::new("coverage"),
            &workspace,
            |_| Err(io::Error::other("coverage directory failed")),
            |_| panic!("scratch creation must not run"),
        )
        .expect_err("coverage-directory failure must propagate");
        assert!(coverage_error.to_string().contains("coverage directory failed"));

        let scratch_error =
            prepare_coverage_directories(Path::new("coverage"), &workspace, |_| Ok(()), |_| Err(app_error("scratch failed")))
                .expect_err("scratch failure must propagate");
        assert_eq!(scratch_error.to_string(), "scratch failed");
    }

    #[test]
    fn no_coverage_target_matching_is_exact() {
        let configured = vec!["aarch64-pc-windows-msvc".to_owned()];
        assert!(configured_no_coverage_target(&configured, "aarch64-pc-windows-msvc"));
        assert!(!configured_no_coverage_target(&configured, "x86_64-pc-windows-msvc"));
        assert!(!configured_no_coverage_target(&[], "aarch64-pc-windows-msvc"));
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
        for (selector, expected) in [
            ("alpha", true),
            ("alpha@1.2.3", true),
            ("a?pha", true),
            ("alpha*", true),
            ("alpha@1.2.4", false),
            ("beta*", false),
        ] {
            let pattern = crate::package_glob::parse(selector).expect("valid selector");
            assert_eq!(selector_matches(selector, &pattern, &alpha), expected, "{selector}");
        }
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
    fn empty_package_selection_is_implicit_and_includes_every_member() {
        let workspace = workspace(Path::new("repo"));
        let selection = Selection::resolve(&workspace, &[]).expect("empty selection");
        assert!(!selection.explicit);
        assert_eq!(
            selection.members.iter().map(WorkspaceMember::spec).collect::<Vec<_>>(),
            ["zeta@2.0.0", "alpha@1.0.0"]
        );
    }

    #[test]
    fn package_selection_errors_encode_terminal_controls() {
        let workspace = workspace(Path::new("repo"));
        for selector in ["lib[\n\r\u{1b}[2J", "missing\n\r\u{1b}[2J"] {
            let error = Selection::resolve(&workspace, &[selector.to_owned()]).expect_err("selector must fail");
            let rendered = error.to_string();
            assert!(!rendered.contains(selector));
            assert!(rendered.contains(r"\n\r\u{1b}[2J"));
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns cargo metadata")]
    fn workspace_info_sorts_members_independently_of_metadata_order() {
        let mut metadata = MetadataCommand::new().no_deps().exec().expect("workspace metadata");
        metadata.packages.reverse();
        let workspace = WorkspaceInfo::from_metadata(&metadata);
        assert!(workspace.members.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn effective_target_reader_uses_exact_context_and_host_error() {
        let workspace = workspace(Path::new("repo"));
        let tools = ToolPrograms {
            cargo: OsString::from("cargo"),
            rustc: OsString::from("rustc"),
        };
        let error = EffectiveTarget::resolve_with_reader(&workspace, None, &tools, |_, _, description| {
            assert_eq!(description, "rustc host-target discovery");
            Ok("rustc 1.99.0-nightly".to_owned())
        })
        .expect_err("missing host must fail");
        assert_eq!(error.to_string(), "`rustc -vV` did not report a host target");
    }

    #[test]
    fn instrumentation_validation_uses_exact_commands_contexts_and_errors() {
        let workspace = workspace(Path::new("repo"));
        let tools = ToolPrograms {
            cargo: OsString::from("cargo-custom"),
            rustc: OsString::from("rustc-custom"),
        };
        let mut calls = Vec::new();
        let mut outputs = [
            "release: 1.99.0-nightly\n".to_owned(),
            "release: 1.99.0-nightly\n".to_owned(),
            "cargo-llvm-cov 0.9.0\n".to_owned(),
        ]
        .into_iter();
        validate_instrumentation_tools_with(&workspace, &tools, None, |command, description| {
            calls.push((
                command.get_program().to_owned(),
                command.get_args().map(OsStr::to_owned).collect::<Vec<_>>(),
                description.to_owned(),
            ));
            Ok(outputs.next().expect("one output per command"))
        })
        .expect("valid tools");
        assert_eq!(
            calls,
            [
                (
                    OsString::from("cargo-custom"),
                    ["--version", "--verbose"].map(OsString::from).to_vec(),
                    "cargo toolchain validation".to_owned(),
                ),
                (
                    OsString::from("rustc-custom"),
                    ["-vV"].map(OsString::from).to_vec(),
                    "rustc toolchain validation".to_owned(),
                ),
                (
                    OsString::from("cargo-custom"),
                    ["llvm-cov", "--version"].map(OsString::from).to_vec(),
                    "cargo-llvm-cov version validation".to_owned(),
                ),
            ]
        );

        for (outputs, expected) in [
            (
                vec!["cargo 1.99.0-nightly\n"],
                "`cargo --version --verbose` did not report a release",
            ),
            (
                vec!["release: 1.99.0\n"],
                "`cargo coverage-gate run` requires nightly Cargo for `cfg(coverage_nightly)`, but the effective Cargo release is `1.99.0`",
            ),
            (
                vec!["release: 1.99.0-nightly\n", "rustc 1.99.0-nightly\n"],
                "`rustc -vV` did not report a release",
            ),
            (
                vec!["release: 1.99.0-nightly\n", "release: 1.99.0\n"],
                "`cargo coverage-gate run` requires nightly rustc for `cfg(coverage_nightly)`, but the effective rustc release is `1.99.0`",
            ),
            (
                vec!["release: 1.99.0-nightly\n", "release: 1.99.0-nightly\n", "not cargo llvm cov\n"],
                "`cargo llvm-cov --version` did not report a cargo-llvm-cov version",
            ),
        ] {
            let mut outputs = outputs.into_iter();
            let error = validate_instrumentation_tools_with(&workspace, &tools, None, |_, _| {
                Ok(outputs.next().expect("output for attempted command").to_owned())
            })
            .expect_err("invalid tool output must fail");
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn instrumentation_reader_errors_propagate_from_each_tool_query() {
        let workspace = workspace(Path::new("repo"));
        let tools = ToolPrograms {
            cargo: OsString::from("cargo"),
            rustc: OsString::from("rustc"),
        };

        let successful_outputs = [
            ("Cargo release query", "release: 1.99.0-nightly\n"),
            ("rustc release query", "release: 1.99.0-nightly\n"),
            ("cargo-llvm-cov version query", "cargo-llvm-cov 0.9.0\n"),
        ];
        for (failed_call, (failed_query, _)) in successful_outputs.iter().enumerate() {
            let mut call = 0;
            let error = validate_instrumentation_tools_with(&workspace, &tools, None, |_, description| {
                let current = call;
                call += 1;
                if current == failed_call {
                    Err(AppError::new(format!("injected {description} failure")))
                } else {
                    Ok(successful_outputs[current].1.to_owned())
                }
            })
            .expect_err("injected reader failure must propagate");
            assert!(
                error.to_string().contains("injected"),
                "failed query {failed_query} at index {failed_call}: {error}"
            );
        }
    }

    #[cfg_attr(miri, ignore = "spawns a missing cargo process")]
    #[test]
    fn workspace_metadata_spawn_errors_are_returned() {
        let missing = format!("missing-cargo-coverage-gate-{}", process_id());
        let tools = ToolPrograms {
            cargo: OsString::from(&missing),
            rustc: OsString::from("rustc"),
        };
        let error = WorkspaceInfo::load(&tools).expect_err("missing cargo must fail").to_string();
        assert!(error.contains(METADATA_LOAD_CONTEXT), "{error}");
    }
    #[test]
    fn plain_collection_command_preserves_selection_target_and_jobs() {
        let workspace = workspace(Path::new("repo"));
        let tools = ToolPrograms {
            cargo: OsString::from("cargo-custom"),
            rustc: OsString::from("rustc-custom"),
        };
        let args = collection_args();
        let implicit = Selection {
            explicit: false,
            members: workspace.members.clone(),
        };

        let plain = plain_configuration_command(
            &workspace,
            &implicit,
            &args,
            FeatureConfiguration::NoDefaultFeatures,
            "test-target",
            &tools,
        );
        assert_eq!(plain.get_program(), OsStr::new("cargo-custom"));
        assert_eq!(plain.get_current_dir(), Some(Path::new("repo")));
        assert_eq!(
            plain.get_args().collect::<Vec<_>>(),
            [
                "nextest",
                "run",
                "--workspace",
                "--no-default-features",
                "--locked",
                "--target",
                "test-target",
                "--jobs",
                "3",
                "--build-jobs",
                "3",
                "--no-tests=pass",
            ]
            .map(OsStr::new)
        );
    }

    #[test]
    #[cfg_attr(
        all(miri, windows),
        ignore = "constructing a Command environment calls an unsupported Windows API under miri"
    )]
    fn coverage_collection_commands_preserve_selection_target_jobs_and_environment() {
        let workspace = workspace(Path::new("repo"));
        let tools = ToolPrograms {
            cargo: OsString::from("cargo-custom"),
            rustc: OsString::from("rustc-custom"),
        };
        let args = collection_args();
        let explicit = Selection {
            explicit: true,
            members: vec![member("alpha", "1.0.0")],
        };
        let coverage_target_root = PathBuf::from("coverage-target");
        let execution = CollectionExecution {
            workspace: &workspace,
            selection: &explicit,
            args: &args,
            #[cfg(windows)]
            scratch_dir: Path::new("scratch"),
            coverage_target_root: &coverage_target_root,
            target: "test-target",
            tools: &tools,
            quiet: false,
        };
        let nextest = nextest_command(&execution, FeatureConfiguration::AllFeatures);
        assert_eq!(
            nextest.get_args().collect::<Vec<_>>(),
            [
                "llvm-cov",
                "nextest",
                "--no-report",
                "--package",
                "alpha@1.0.0",
                "--all-features",
                "--locked",
                "--target",
                "test-target",
                "--jobs",
                "3",
                "--build-jobs",
                "3",
                "--no-tests=pass",
            ]
            .map(OsStr::new)
        );
        let env = nextest
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
            .collect::<std::collections::BTreeMap<_, _>>();
        let expected_target = PathBuf::from("coverage-target").join("all-features").into_os_string();
        assert_eq!(env.get(OsStr::new("CARGO_TARGET_DIR")), Some(&Some(expected_target.clone())));
        assert_eq!(
            env.get(OsStr::new("CARGO_LLVM_COV_TARGET_DIR")),
            Some(&Some(expected_target.clone()))
        );
        assert_eq!(env.get(OsStr::new("CARGO_LLVM_COV_BUILD_DIR")), Some(&Some(expected_target)));

        let report = report_command(&execution, FeatureConfiguration::NoDefaultFeatures, Path::new("coverage/lcov.info"));
        assert_eq!(
            report.get_args().collect::<Vec<_>>(),
            [
                "llvm-cov",
                "report",
                "--lcov",
                "--output-path",
                "coverage/lcov.info",
                "--package",
                "alpha@1.0.0",
                "--target",
                "test-target",
            ]
            .map(OsStr::new)
        );
    }

    #[test]
    fn glob_matching_covers_empty_and_character_class_branches() {
        assert!(crate::package_glob::parse("*").expect("valid glob").matches(""));
        assert!(crate::package_glob::parse("lib[12]").expect("valid glob").matches("lib1"));
        assert!(!crate::package_glob::parse("a?").expect("valid glob").matches("a"));
        assert!(!crate::package_glob::parse("a*b").expect("valid glob").matches("ac"));
    }

    #[test]
    fn command_too_long_parser_preserves_windows_export_arguments() {
        let stderr = concat!(
            "error: failed to generate report: could not execute process `",
            "\"C:\\Program Files\\Rust\\llvm-cov.exe\" export -format=lcov ",
            "-object \"C:\\coverage objects\\say \\\"quoted\\\"\\object.exe\" ",
            "-ignore-filename-regex UPSTREAM_DEFAULTS",
            "` (never executed): The filename or extension is too long. (os error 206)",
        );
        let arguments = command_too_long_response_arguments(stderr).expect("Windows error 206 export");
        assert_eq!(
            arguments,
            [
                "-format=lcov",
                "-object",
                r#"C:\coverage objects\say "quoted"\object.exe"#,
                "-ignore-filename-regex",
                "UPSTREAM_DEFAULTS",
            ]
        );

        let response = windows_response_contents(&arguments).expect("encode response file");
        assert_eq!(
            parse_windows_command_line(std::str::from_utf8(&response).expect("response is UTF-8")).expect("parse response file"),
            arguments
        );
        assert_eq!(
            command_too_long_response_arguments(
                "could not execute process `llvm-cov export only-argument` (never executed) (os error 206)"
            )
            .expect("one export argument is valid"),
            ["only-argument"]
        );
    }

    #[test]
    fn windows_command_too_long_detection_is_exact() {
        assert!(is_windows_command_too_long(b"failed with (os error 206)"));
        assert!(!is_windows_command_too_long(b"failed with (os error 20)"));
        assert!(!is_windows_command_too_long(b"unrelated failure"));
    }

    #[test]
    fn command_too_long_parser_rejects_ambiguous_diagnostics() {
        for (diagnostic, expected) in [
            ("unrelated error", "cargo-llvm-cov did not report Windows error 206"),
            (
                "report failed (os error 206)",
                "cargo-llvm-cov's Windows command-too-long diagnostic did not contain the expected process-error prefix",
            ),
            (
                "could not execute process `llvm-cov export arg (os error 206)",
                "cargo-llvm-cov's Windows command-too-long diagnostic did not contain the expected process-error suffix",
            ),
            (
                "could not execute process ` export arg` (never executed) (os error 206)",
                "cargo-llvm-cov's Windows command-too-long diagnostic did not contain an llvm-cov export command",
            ),
            (
                "could not execute process `\"\" export arg` (never executed) (os error 206)",
                "cargo-llvm-cov's Windows command-too-long diagnostic did not contain an llvm-cov export command",
            ),
            (
                "could not execute process `llvm-cov show arg` (never executed) (os error 206)",
                "cargo-llvm-cov's Windows command-too-long diagnostic did not contain an llvm-cov export command",
            ),
            (
                "could not execute process `llvm-cov show` (never executed) (os error 206)",
                "cargo-llvm-cov's Windows command-too-long diagnostic did not contain an llvm-cov export command",
            ),
            (
                "could not execute process `llvm-cov export \"unterminated` (never executed) (os error 206)",
                "cargo-llvm-cov's Windows command-too-long diagnostic contained an unterminated quoted argument",
            ),
            (
                "could not execute process `llvm-cov export` (never executed) (os error 206)",
                "cargo-llvm-cov's Windows command-too-long diagnostic did not contain an llvm-cov export command",
            ),
            (
                "could not execute process `llvm-cov export arg\0` (never executed) (os error 206)",
                "cargo-llvm-cov's Windows command-too-long diagnostic contained a null byte",
            ),
        ] {
            assert_eq!(
                command_too_long_response_arguments(diagnostic)
                    .expect_err("ambiguous diagnostic must fail")
                    .to_string(),
                expected
            );
        }
        assert_eq!(
            windows_response_contents(&[])
                .expect_err("an empty response file must fail")
                .to_string(),
            "cargo-llvm-cov's Windows command-too-long diagnostic contained no export arguments"
        );
        assert_eq!(
            windows_response_contents(&["bad\0argument".to_owned()])
                .expect_err("null bytes must fail")
                .to_string(),
            "cargo-llvm-cov's Windows command-too-long diagnostic contained a null byte"
        );
    }

    #[test]
    fn windows_argument_quoting_preserves_each_backslash_position() {
        let arguments = [
            String::from(r"plain\path"),
            String::from(r#"before\"quote"#),
            String::from(r"trailing\\"),
        ];
        let response = windows_response_contents(&arguments).expect("response encoding");
        assert_eq!(
            parse_windows_command_line(std::str::from_utf8(&response).expect("UTF-8")).expect("response parsing"),
            arguments
        );
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
    fn command_display_preserves_argument_boundaries() {
        let mut command = Command::new("cargo");
        command.args(["llvm-cov", "--output-path", "coverage output/report.info", ""]);

        assert_eq!(
            command_display(&command),
            r#"cargo llvm-cov --output-path "coverage output/report.info" """#
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns a process")]
    fn read_stdout_captures_the_child_output() {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("cmd");
            command.args(["/C", "echo captured-output"]);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "printf captured-output"]);
            command
        };
        assert!(
            read_stdout(&mut command, "capture test")
                .expect("command succeeds")
                .contains("captured-output")
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "attempts to spawn a missing process")]
    fn process_spawn_errors_include_the_command() {
        let missing = format!("missing-coverage-gate-command-{}", process_id());
        let mut command = Command::new(&missing);
        let read_error = read_stdout(&mut command, "missing command")
            .expect_err("missing command must fail")
            .to_string();
        assert!(read_error.contains(&missing), "{read_error}");

        let mut command = Command::new(&missing);
        let status_error = run_status(&mut command, "missing command")
            .expect_err("missing command must fail")
            .to_string();
        assert!(status_error.contains(&missing), "{status_error}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "spawns a process")]
    fn read_stdout_returns_wait_errors() {
        #[cfg(windows)]
        let mut command = Command::new("cmd");
        #[cfg(windows)]
        command.args(["/C", "exit", "0"]);
        #[cfg(not(windows))]
        let mut command = Command::new("sh");
        #[cfg(not(windows))]
        command.args(["-c", "exit 0"]);

        let error = read_stdout_with(&mut command, "wait failure test", |_| {
            Err(io::Error::other("injected wait failure"))
        })
        .expect_err("wait failure must propagate")
        .to_string();
        assert!(error.contains("failed to wait for"), "{error}");
        assert!(error.contains("injected wait failure"), "{error}");
    }
    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("injected"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn forwarding_errors_have_exact_context() {
        let output = process_output(true, b"stdout", b"stderr");
        let stdout_error = forward_stdout_to(&output, false, &mut FailingWriter)
            .expect_err("stdout forwarding must fail")
            .to_string();
        assert!(stdout_error.contains("failed to forward cargo-llvm-cov stdout"), "{stdout_error}");
        let stderr_error = forward_output_to(&output, true, &mut io::sink(), &mut FailingWriter)
            .expect_err("stderr forwarding must fail")
            .to_string();
        assert!(stderr_error.contains("failed to forward cargo-llvm-cov stderr"), "{stderr_error}");

        let stdout_error = forward_output_to(&output, false, &mut FailingWriter, &mut io::sink())
            .expect_err("stdout forwarding failure must propagate through combined forwarding")
            .to_string();
        assert!(stdout_error.contains("failed to forward cargo-llvm-cov stdout"), "{stdout_error}");
    }

    #[test]
    fn forwarding_obeys_quiet_and_preserves_both_streams() {
        let output = process_output(true, b"stdout", b"stderr");

        for (quiet, expected_stdout) in [(false, b"stdout".as_slice()), (true, b"".as_slice())] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            forward_output_to(&output, quiet, &mut stdout, &mut stderr).expect("forward output");
            assert_eq!(stdout, expected_stdout, "quiet={quiet}");
            assert_eq!(stderr, b"stderr", "quiet={quiet}");
        }
    }

    #[test]
    fn stdout_forwarder_passes_quiet_through_unchanged() {
        let output = process_output(true, b"stdout", b"");
        let mut stdout = Vec::new();
        {
            let mut forward = stdout_forwarder(&mut stdout);
            forward(&output, true).expect("quiet forwarding succeeds");
        }
        assert!(stdout.is_empty(), "quiet fallback forwarding must suppress stdout");
        {
            let mut forward = stdout_forwarder(&mut stdout);
            forward(&output, false).expect("non-quiet forwarding succeeds");
        }
        assert_eq!(stdout, b"stdout", "non-quiet fallback forwarding must preserve stdout");
    }

    #[test]
    fn report_output_handling_propagates_each_boundary_error_and_quiet_value() {
        for quiet in [false, true] {
            let output = process_output(true, b"out", b"err");
            let mut seen_quiet = None;
            let action = handle_report_output_with(
                &output,
                quiet,
                "cargo report",
                Path::new("lcov.info"),
                false,
                |_, actual_quiet| {
                    seen_quiet = Some(actual_quiet);
                    Ok(())
                },
                |_, _| panic!("success must not use the no-data stdout path"),
                |_| panic!("success must not write an empty report"),
            )
            .expect("successful report output");
            assert!(matches!(action, ReportAction::Complete));
            assert_eq!(seen_quiet, Some(quiet));
        }

        let output = process_output(true, b"", b"");
        let forward_error = handle_report_output_with(
            &output,
            false,
            "cargo report",
            Path::new("lcov.info"),
            false,
            |_, _| Err(app_error("forward failed")),
            |_, _| panic!("success must not use the no-data stdout path"),
            |_| panic!("success must not write an empty report"),
        )
        .expect_err("forwarding failure must propagate");
        assert_eq!(forward_error.to_string(), "forward failed");

        let no_data = process_output(
            false,
            b"partial",
            b"error: no coverage data found\nerror: could not load coverage information\n",
        );
        let stdout_error = handle_report_output_with(
            &no_data,
            true,
            "cargo report",
            Path::new("lcov.info"),
            false,
            |_, _| panic!("no-data output must not use ordinary forwarding"),
            |_, quiet| {
                assert!(quiet);
                Err(app_error("stdout failed"))
            },
            |_| panic!("write must not follow stdout failure"),
        )
        .expect_err("stdout failure must propagate");
        assert_eq!(stdout_error.to_string(), "stdout failed");

        let write_error = handle_report_output_with(
            &no_data,
            false,
            "cargo report",
            Path::new("lcov.info"),
            false,
            |_, _| panic!("no-data output must not use ordinary forwarding"),
            |_, quiet| {
                assert!(!quiet);
                Ok(())
            },
            |path| {
                assert_eq!(path, Path::new("lcov.info"));
                Err(app_error("empty write failed"))
            },
        )
        .expect_err("empty-report write failure must propagate");
        assert_eq!(write_error.to_string(), "empty write failed");

        let failed = process_output(false, b"", b"ordinary failure");
        let ordinary_forward_error = handle_report_output_with(
            &failed,
            true,
            "cargo report",
            Path::new("lcov.info"),
            false,
            |_, quiet| {
                assert!(quiet);
                Err(app_error("failure forwarding failed"))
            },
            |_, _| panic!("ordinary failure must not use no-data forwarding"),
            |_| panic!("ordinary failure must not write a report"),
        )
        .expect_err("failure forwarding error must propagate");
        assert_eq!(ordinary_forward_error.to_string(), "failure forwarding failed");
    }

    #[test]
    fn windows_overflow_output_handling_returns_parse_and_forward_errors() {
        let diagnostic = concat!(
            "error: could not execute process `",
            "llvm-cov export -object object.exe",
            "` (never executed): too long (os error 206)"
        );
        let output = process_output(false, b"", diagnostic.as_bytes());
        let action = handle_report_output_with(
            &output,
            false,
            "cargo report",
            Path::new("lcov.info"),
            true,
            |_, quiet| {
                assert!(!quiet);
                Ok(())
            },
            |_, _| panic!("overflow must not use no-data forwarding"),
            |_| panic!("overflow must not write an empty report"),
        )
        .expect("valid overflow diagnostic");
        let ReportAction::WindowsFallback(arguments) = action else {
            panic!("overflow must request fallback");
        };
        assert_eq!(arguments, ["-object", "object.exe"]);

        let forward_error = handle_report_output_with(
            &output,
            true,
            "cargo report",
            Path::new("lcov.info"),
            true,
            |_, quiet| {
                assert!(quiet);
                Err(app_error("overflow forwarding failed"))
            },
            |_, _| panic!("overflow must not use no-data forwarding"),
            |_| panic!("overflow must not write an empty report"),
        )
        .expect_err("overflow forwarding failure must propagate");
        assert_eq!(forward_error.to_string(), "overflow forwarding failed");

        let mut forwarded = false;
        let disabled_error = handle_report_output_with(
            &output,
            false,
            "cargo report",
            Path::new("lcov.info"),
            false,
            |_, _| {
                forwarded = true;
                Ok(())
            },
            |_, _| panic!("ordinary failure must not use no-data forwarding"),
            |_| panic!("ordinary failure must not write an empty report"),
        )
        .expect_err("disabled Windows fallback must preserve the ordinary failure");
        assert!(forwarded);
        assert!(disabled_error.to_string().contains("cargo llvm-cov report failed"));

        let invalid_utf8 = process_output(
            false,
            b"",
            b"could not execute process `llvm-cov export x` (never executed) (os error 206)\xFF",
        );
        let utf8_error = handle_report_output_with(
            &invalid_utf8,
            false,
            "cargo report",
            Path::new("lcov.info"),
            true,
            |_, _| Ok(()),
            |_, _| panic!("overflow must not use no-data forwarding"),
            |_| panic!("overflow must not write an empty report"),
        )
        .expect_err("invalid UTF-8 must propagate");
        assert!(utf8_error.to_string().contains(WINDOWS_DIAGNOSTIC_UTF8_CONTEXT));

        let malformed = process_output(false, b"", b"(os error 206)");
        let parse_error = handle_report_output_with(
            &malformed,
            false,
            "cargo report",
            Path::new("lcov.info"),
            true,
            |_, _| Ok(()),
            |_, _| panic!("overflow must not use no-data forwarding"),
            |_| panic!("overflow must not write an empty report"),
        )
        .expect_err("malformed overflow diagnostic must propagate");
        assert!(parse_error.to_string().contains("process-error prefix"));
    }

    #[cfg_attr(miri, ignore = "attempts to spawn a missing process")]
    #[test]
    fn report_spawn_errors_include_the_complete_command() {
        let workspace = workspace(Path::new("."));
        let args = collection_args();
        let selection = Selection {
            explicit: false,
            members: workspace.members.clone(),
        };
        let missing = format!("missing-report-command-{}", process_id());
        let tools = ToolPrograms {
            cargo: OsString::from(&missing),
            rustc: OsString::from("rustc"),
        };
        let coverage_target_root = PathBuf::from("coverage-target");
        let execution = CollectionExecution {
            workspace: &workspace,
            selection: &selection,
            args: &args,
            #[cfg(windows)]
            scratch_dir: Path::new("scratch"),
            coverage_target_root: &coverage_target_root,
            target: "test-target",
            tools: &tools,
            quiet: false,
        };

        let error = run_report(&execution, FeatureConfiguration::AllFeatures, Path::new("lcov.info"))
            .expect_err("missing report command must fail")
            .to_string();
        assert!(error.contains(&missing), "{error}");
        assert!(error.contains("llvm-cov report"), "{error}");
    }

    #[test]
    fn llvm_cov_discovery_uses_exact_rustc_query_and_bin_path() {
        let tools = ToolPrograms {
            cargo: OsString::from("cargo"),
            rustc: OsString::from("rustc-custom"),
        };
        let target_libdir = PathBuf::from("rust").join("lib").join("rustlib").join("target").join("lib");
        let discovered = discover_llvm_cov_with(
            &tools,
            |name| {
                assert_eq!(name, "LLVM_COV");
                None
            },
            |command, description| {
                assert_eq!(command.get_program(), OsStr::new("rustc-custom"));
                assert_eq!(command.get_args().collect::<Vec<_>>(), ["--print", "target-libdir"].map(OsStr::new));
                assert_eq!(description, "rustc LLVM-tool discovery");
                Ok(target_libdir.to_string_lossy().into_owned())
            },
        )
        .expect("discovery succeeds");
        assert_eq!(
            discovered,
            target_libdir
                .parent()
                .expect("fixture target libdir has a parent")
                .join("bin")
                .join(format!("llvm-cov{}", env::consts::EXE_SUFFIX))
        );
    }

    #[test]
    fn llvm_cov_discovery_prefers_the_environment_override() {
        let tools = ToolPrograms {
            cargo: OsString::from("cargo"),
            rustc: OsString::from("rustc"),
        };
        let expected = PathBuf::from("custom-llvm-cov");
        let discovered = discover_llvm_cov_with(
            &tools,
            |name| {
                assert_eq!(name, "LLVM_COV");
                Some(expected.clone().into_os_string())
            },
            |_, _| panic!("the override must avoid rustc discovery"),
        )
        .expect("the override is a complete discovery result");

        assert_eq!(discovered, expected);
    }

    #[test]
    fn llvm_cov_discovery_reports_target_libdir_without_parent() {
        let tools = ToolPrograms {
            cargo: OsString::from("cargo"),
            rustc: OsString::from("rustc"),
        };
        let error = discover_llvm_cov_with(&tools, |_| None, |_, _| Ok(String::new())).expect_err("an empty target-libdir has no parent");
        assert_eq!(error.to_string(), RUSTC_TARGET_LIBDIR_PARENT_ERROR);
    }

    #[test]
    fn llvm_cov_discovery_propagates_rustc_reader_errors() {
        let tools = ToolPrograms {
            cargo: OsString::from("cargo"),
            rustc: OsString::from("rustc"),
        };
        let error = discover_llvm_cov_with(&tools, |_| None, |_, _| Err(app_error("rustc read failed")))
            .expect_err("rustc reader failure must propagate");
        assert_eq!(error.to_string(), "rustc read failed");
    }
    #[test]
    fn windows_export_fallback_uses_response_file_argument() {
        let mut command = Command::new("llvm-cov-custom");
        configure_windows_export_command(&mut command, Path::new(r"C:\workspace"), Path::new(r"C:\scratch\objects.rsp"));
        assert_eq!(command.get_current_dir(), Some(Path::new(r"C:\workspace")));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["export", r"@C:\scratch\objects.rsp"].map(OsStr::new)
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files and process exit statuses")]
    #[expect(
        clippy::too_many_lines,
        reason = "one table-driven boundary test keeps the fallback pipeline and its short-circuit expectations together"
    )]
    fn windows_fallback_returns_every_injected_boundary_error() {
        let tmp = tempdir().expect("tempdir");
        let workspace = workspace(tmp.path());
        let selection = Selection {
            explicit: false,
            members: workspace.members.clone(),
        };
        let args = collection_args();
        let tools = ToolPrograms {
            cargo: OsString::from("cargo"),
            rustc: OsString::from("rustc"),
        };
        let coverage_target_root = tmp.path().join("coverage-target");
        let execution = CollectionExecution {
            workspace: &workspace,
            selection: &selection,
            args: &args,
            #[cfg(windows)]
            scratch_dir: tmp.path(),
            coverage_target_root: &coverage_target_root,
            target: "test-target",
            tools: &tools,
            quiet: true,
        };
        let arguments = vec!["-object".to_owned(), "object.exe".to_owned()];
        let lcov_path = tmp.path().join("lcov.info");

        let encode_error = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Err(app_error("encode failed")),
            |_, _, _| panic!("response write must not run"),
            |_, _| panic!("staging must not run"),
            |_| panic!("discovery must not run"),
            |_, _, _, _| panic!("execution must not run"),
            |_, _| panic!("forwarding must not run"),
            |_, _| panic!("stdout forwarding must not run"),
            |_| panic!("empty write must not run"),
            |_, _| panic!("publish must not run"),
        )
        .expect_err("encoding failure must propagate");
        assert_eq!(encode_error.to_string(), "encode failed");

        let write_error = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            |_, _, _| Err(app_error("response write failed")),
            |_, _| panic!("staging must not run"),
            |_| panic!("discovery must not run"),
            |_, _, _, _| panic!("execution must not run"),
            |_, _| panic!("forwarding must not run"),
            |_, _| panic!("stdout forwarding must not run"),
            |_| panic!("empty write must not run"),
            |_, _| panic!("publish must not run"),
        )
        .expect_err("response write failure must propagate");
        assert_eq!(write_error.to_string(), "response write failed");

        let no_parent = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            Path::new(""),
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            |_, _| panic!("staging must not run"),
            |_| panic!("discovery must not run"),
            |_, _, _, _| panic!("execution must not run"),
            |_, _| panic!("forwarding must not run"),
            |_, _| panic!("stdout forwarding must not run"),
            |_| panic!("empty write must not run"),
            |_, _| panic!("publish must not run"),
        )
        .expect_err("parentless LCOV path must fail");
        assert!(no_parent.to_string().contains("has no parent directory"));

        let staged_error = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            |_, _| Err(app_error("staging failed")),
            |_| panic!("discovery must not run"),
            |_, _, _, _| panic!("execution must not run"),
            |_, _| panic!("forwarding must not run"),
            |_, _| panic!("stdout forwarding must not run"),
            |_| panic!("empty write must not run"),
            |_, _| panic!("publish must not run"),
        )
        .expect_err("staging failure must propagate");
        assert_eq!(staged_error.to_string(), "staging failed");

        let discover_error = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            TemporaryPath::create,
            |_| Err(app_error("discovery failed")),
            |_, _, _, _| panic!("execution must not run"),
            |_, _| panic!("forwarding must not run"),
            |_, _| panic!("stdout forwarding must not run"),
            |_| panic!("empty write must not run"),
            |_, _| panic!("publish must not run"),
        )
        .expect_err("discovery failure must propagate");
        assert_eq!(discover_error.to_string(), "discovery failed");

        let execute_error = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            TemporaryPath::create,
            |_| Ok(PathBuf::from("llvm-cov")),
            |_, _, _, _| Err(app_error("execution failed")),
            |_, _| panic!("forwarding must not run"),
            |_, _| panic!("stdout forwarding must not run"),
            |_| panic!("empty write must not run"),
            |_, _| panic!("publish must not run"),
        )
        .expect_err("execution failure must propagate");
        assert_eq!(execute_error.to_string(), "execution failed");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files and process exit statuses")]
    #[expect(
        clippy::too_many_lines,
        reason = "one table-driven output test keeps success, no-data, and failure forwarding contracts together"
    )]
    fn windows_fallback_output_paths_preserve_quiet_and_errors() {
        let tmp = tempdir().expect("tempdir");
        let workspace = workspace(tmp.path());
        let selection = Selection {
            explicit: false,
            members: workspace.members.clone(),
        };
        let args = collection_args();
        let tools = ToolPrograms {
            cargo: OsString::from("cargo"),
            rustc: OsString::from("rustc"),
        };
        let coverage_target_root = tmp.path().join("coverage-target");
        let execution = CollectionExecution {
            workspace: &workspace,
            selection: &selection,
            args: &args,
            #[cfg(windows)]
            scratch_dir: tmp.path(),
            coverage_target_root: &coverage_target_root,
            target: "test-target",
            tools: &tools,
            quiet: true,
        };
        let arguments = vec!["-object".to_owned(), "object.exe".to_owned()];
        let lcov_path = tmp.path().join("lcov.info");

        let success = process_output(true, b"lcov", b"warning");
        let mut published = false;
        run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            TemporaryPath::create,
            |_| Ok(PathBuf::from("llvm-cov")),
            |_, _, _, _| Ok(("llvm-cov export".to_owned(), success)),
            |_, quiet| {
                assert!(quiet);
                Ok(())
            },
            |_, _| panic!("successful export must not use no-data forwarding"),
            |_| panic!("successful export must not write an empty report"),
            |_, target| {
                assert_eq!(target, lcov_path);
                published = true;
                Ok(())
            },
        )
        .expect("successful fallback");
        assert!(published);

        let success = process_output(true, b"lcov", b"warning");
        let success_forward_error = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            TemporaryPath::create,
            |_| Ok(PathBuf::from("llvm-cov")),
            |_, _, _, _| Ok(("llvm-cov export".to_owned(), success)),
            |_, quiet| {
                assert!(quiet);
                Err(app_error("success forwarding failed"))
            },
            |_, _| panic!("successful export must not use no-data forwarding"),
            |_| panic!("successful export must not write an empty report"),
            |_, _| panic!("publish must not follow forwarding failure"),
        )
        .expect_err("successful-output forwarding failure must propagate");
        assert_eq!(success_forward_error.to_string(), "success forwarding failed");

        let no_data = process_output(
            false,
            b"partial",
            b"error: no coverage data found\nerror: could not load coverage information\n",
        );
        let stdout_error = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            TemporaryPath::create,
            |_| Ok(PathBuf::from("llvm-cov")),
            |_, _, _, _| Ok(("llvm-cov export".to_owned(), no_data)),
            |_, _| panic!("no-data export must not use ordinary forwarding"),
            |_, quiet| {
                assert!(quiet);
                Err(app_error("stdout failed"))
            },
            |_| panic!("write must not follow stdout failure"),
            |_, _| panic!("publish must not follow stdout failure"),
        )
        .expect_err("stdout failure must propagate");
        assert_eq!(stdout_error.to_string(), "stdout failed");

        let no_data = process_output(
            false,
            b"partial",
            b"error: no coverage data found\nerror: could not load coverage information\n",
        );
        let write_error = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            TemporaryPath::create,
            |_| Ok(PathBuf::from("llvm-cov")),
            |_, _, _, _| Ok(("llvm-cov export".to_owned(), no_data)),
            |_, _| panic!("no-data export must not use ordinary forwarding"),
            |_, quiet| {
                assert!(quiet);
                Ok(())
            },
            |_| Err(app_error("empty write failed")),
            |_, _| panic!("publish must not follow write failure"),
        )
        .expect_err("empty write failure must propagate");
        assert_eq!(write_error.to_string(), "empty write failed");

        let no_data = process_output(
            false,
            b"partial",
            b"error: no coverage data found\nerror: could not load coverage information\n",
        );
        let publish_error = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            TemporaryPath::create,
            |_| Ok(PathBuf::from("llvm-cov")),
            |_, _, _, _| Ok(("llvm-cov export".to_owned(), no_data)),
            |_, _| panic!("no-data export must not use ordinary forwarding"),
            |_, quiet| {
                assert!(quiet);
                Ok(())
            },
            |_| Ok(()),
            |_, _| Err(app_error("publish failed")),
        )
        .expect_err("publish failure must propagate");
        assert_eq!(publish_error.to_string(), "publish failed");

        let no_data = process_output(
            false,
            b"partial",
            b"error: no coverage data found\nerror: could not load coverage information\n",
        );
        let mut stdout_forwarded = false;
        let mut empty_written = false;
        let mut published = false;
        run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            TemporaryPath::create,
            |_| Ok(PathBuf::from("llvm-cov")),
            |_, _, _, _| Ok(("llvm-cov export".to_owned(), no_data)),
            |_, _| panic!("no-data export must not use ordinary forwarding"),
            |_, quiet| {
                assert!(quiet);
                stdout_forwarded = true;
                Ok(())
            },
            |_| {
                empty_written = true;
                Ok(())
            },
            |_, target| {
                assert_eq!(target, lcov_path);
                published = true;
                Ok(())
            },
        )
        .expect("no-data fallback succeeds");
        assert!(stdout_forwarded);
        assert!(empty_written);
        assert!(published);

        let failed = process_output(false, b"partial", b"ordinary failure");
        let forward_error = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            TemporaryPath::create,
            |_| Ok(PathBuf::from("llvm-cov")),
            |_, _, _, _| Ok(("llvm-cov export".to_owned(), failed)),
            |_, quiet| {
                assert!(quiet);
                Err(app_error("forward failed"))
            },
            |_, _| panic!("ordinary failure must not use no-data forwarding"),
            |_| panic!("ordinary failure must not write an empty report"),
            |_, _| panic!("ordinary failure must not publish"),
        )
        .expect_err("forwarding failure must propagate");
        assert_eq!(forward_error.to_string(), "forward failed");

        let failed = process_output(false, b"partial", b"ordinary failure");
        let mut forwarded = false;
        let export_error = run_windows_report_fallback_with(
            &execution,
            tmp.path(),
            FeatureConfiguration::AllFeatures,
            &lcov_path,
            &arguments,
            |_| Ok(Vec::new()),
            TemporaryPath::write,
            TemporaryPath::create,
            |_| Ok(PathBuf::from("llvm-cov")),
            |_, _, _, _| Ok(("llvm-cov export".to_owned(), failed)),
            |_, quiet| {
                assert!(quiet);
                forwarded = true;
                Ok(())
            },
            |_, _| panic!("ordinary failure must not use no-data forwarding"),
            |_| panic!("ordinary failure must not write an empty report"),
            |_, _| panic!("ordinary failure must not publish"),
        )
        .expect_err("failed export must be reported");
        assert!(forwarded);
        assert!(
            export_error
                .to_string()
                .contains("llvm-cov export response-file fallback failed: `llvm-cov export` exited with"),
            "{export_error}"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files and attempts to spawn a missing process")]
    fn windows_export_execution_errors_include_the_command() {
        let tmp = tempdir().expect("tempdir");
        let output = fs::File::create(tmp.path().join("output.info")).expect("output file");
        let missing = tmp.path().join("missing-llvm-cov");
        let error = execute_windows_export(missing.clone(), tmp.path(), Path::new("objects.rsp"), output)
            .expect_err("missing llvm-cov must fail")
            .to_string();
        assert!(error.contains(&missing.display().to_string()), "{error}");
        assert!(error.contains("@objects.rsp"), "{error}");
    }
    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn temporary_response_file_writes_and_cleans_up_on_drop() {
        let tmp = tempdir().expect("tempdir");
        let temporary = TemporaryPath::write(tmp.path(), "response", b"complete bytes").expect("write temporary response");
        let path = temporary.path().to_path_buf();

        assert_eq!(fs::read(&path).expect("read temporary response"), b"complete bytes");
        drop(temporary);
        assert!(!path.exists());
    }

    #[test]
    fn temporary_paths_reserve_distinct_sequence_numbers() {
        let mut first = TemporaryPath::new(Path::new("scratch"), "response");
        let mut second = TemporaryPath::new(Path::new("scratch"), "response");
        assert_ne!(first.path(), second.path());
        first.armed = false;
        second.armed = false;
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn temporary_file_creation_errors_are_returned() {
        let tmp = tempdir().expect("tempdir");
        let missing = tmp.path().join("missing");
        let create_error = TemporaryPath::create(&missing, "response")
            .expect_err("missing parent must reject temporary creation")
            .to_string();
        assert!(create_error.contains("failed to create temporary file"), "{create_error}");

        let write_error = TemporaryPath::write(&missing, "response", b"bytes")
            .expect_err("write must propagate temporary creation failure")
            .to_string();
        assert!(write_error.contains("failed to create temporary file"), "{write_error}");
    }

    /// Test double that independently injects temporary-file write and durability failures.
    struct ControlledTemporaryWriter {
        fail_write: bool,
        fail_sync: bool,
        bytes: Vec<u8>,
    }

    impl io::Write for ControlledTemporaryWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                return Err(io::Error::other("injected write failure"));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SynchronizedWrite for ControlledTemporaryWriter {
        fn sync_all(&self) -> io::Result<()> {
            if self.fail_sync {
                Err(io::Error::other("injected sync failure"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn temporary_file_write_and_sync_errors_are_returned() {
        let temporary = TemporaryPath {
            path: PathBuf::from("temporary-response"),
            armed: false,
        };
        for (fail_write, fail_sync, expected) in [
            (true, false, "failed to write temporary file"),
            (false, true, "failed to flush temporary file"),
        ] {
            let mut writer = ControlledTemporaryWriter {
                fail_write,
                fail_sync,
                bytes: Vec::new(),
            };
            let error = write_temporary_contents(&temporary, &mut writer, b"complete bytes")
                .expect_err("injected temporary write failure")
                .to_string();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn temporary_write_returns_content_errors() {
        let temporary = TemporaryPath {
            path: PathBuf::from("temporary-response"),
            armed: false,
        };
        let mut writer = ControlledTemporaryWriter {
            fail_write: true,
            fail_sync: false,
            bytes: Vec::new(),
        };

        let error = TemporaryPath::finish_write(temporary, &mut writer, b"complete bytes")
            .expect_err("content write failure must propagate from TemporaryPath::write")
            .to_string();
        assert!(error.contains("failed to write temporary file"), "{error}");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn temporary_publication_replaces_only_with_completed_file() {
        let tmp = tempdir().expect("tempdir");
        let target = tmp.path().join("lcov.info");
        fs::write(&target, b"previous").expect("write previous report");
        TemporaryPath::write(tmp.path(), "completed", b"completed")
            .expect("write staged report")
            .publish(&target)
            .expect("publish completed report");
        assert_eq!(fs::read(&target).expect("read published report"), b"completed");

        let blocked_target = tmp.path().join("directory");
        fs::create_dir(&blocked_target).expect("create blocking directory");
        let staged = TemporaryPath::write(tmp.path(), "blocked", b"partial").expect("write blocked report");
        let staged_path = staged.path().to_path_buf();
        staged.publish(&blocked_target).expect_err("publication over a directory must fail");
        assert!(blocked_target.is_dir());
        assert!(!staged_path.exists(), "failed publication must clean its staging file");
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
    #[cfg_attr(miri, ignore = "uses temporary files and directories")]
    fn isolated_target_parent_creation_errors_are_returned() {
        let tmp = tempdir().expect("tempdir");
        let parent = tmp.path().join("not-a-directory");
        fs::write(&parent, b"file").expect("write blocking file");

        let error = TemporaryDirectory::create(&parent)
            .expect_err("a file cannot be a coverage target parent")
            .to_string();
        assert!(error.contains("failed to create coverage target parent"), "{error}");
    }

    #[test]
    fn isolated_target_allocation_retries_collisions_and_reports_failures() {
        let parent = Path::new("coverage-target-parent");
        let mut sequence = 0_u64;
        let collisions = Cell::new(0);
        let mut allocated = TemporaryDirectory::allocate(
            parent,
            || {
                let current = sequence;
                sequence += 1;
                current
            },
            |path| {
                if path.ends_with(format!("run-{}-0", process_id())) {
                    collisions.set(collisions.get() + 1);
                    Err(io::Error::new(io::ErrorKind::AlreadyExists, "collision"))
                } else {
                    Ok(())
                }
            },
        )
        .expect("allocation retries a collision");
        assert_eq!(collisions.get(), 1);
        assert!(allocated.path().ends_with(format!("run-{}-1", process_id())));
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

        let collision_attempts = Cell::new(0);
        TemporaryDirectory::allocate(
            parent,
            || 9,
            |_| {
                collision_attempts.set(collision_attempts.get() + 1);
                Err(io::Error::new(io::ErrorKind::AlreadyExists, "collision"))
            },
        )
        .expect_err("one hundred collisions must exhaust allocation");
        assert_eq!(collision_attempts.get(), 100);
    }

    #[test]
    fn temporary_sequences_advance_by_exactly_one() {
        let sequence = AtomicU64::new(41);

        assert_eq!(reserve_temporary_sequence(&sequence), 41);
        assert_eq!(reserve_temporary_sequence(&sequence), 42);
        assert_eq!(sequence.load(Ordering::Relaxed), 43);
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files and directories")]
    fn disarmed_temporary_paths_and_directories_are_preserved() {
        let tmp = tempdir().expect("tempdir");
        let file = tmp.path().join("preserved-file");
        fs::write(&file, b"preserved").expect("write file");
        drop(TemporaryPath {
            path: file.clone(),
            armed: false,
        });
        assert_eq!(fs::read(&file).expect("file remains"), b"preserved");

        let directory = tmp.path().join("preserved-directory");
        fs::create_dir(&directory).expect("create directory");
        drop(TemporaryDirectory {
            path: directory.clone(),
            armed: false,
        });
        assert!(directory.is_dir());
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

    #[test]
    fn relative_path_resolution_error_has_exact_context() {
        let error = absolute_path_with(Path::new("coverage"), || Err(io::Error::other("injected")))
            .expect_err("current-directory failure must surface");
        assert!(error.to_string().contains("failed to resolve the current directory"));
    }
}
