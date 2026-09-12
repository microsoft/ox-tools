// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Portable coverage collection for `cargo coverage-gate run`.

use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use cargo_metadata::{Metadata, MetadataCommand};
use ohno::{AppError, IntoAppError};
use semver::Version;
use serde_json::Value;

use crate::cli::{CollectionArgs, CoverageGateArgs, FeatureConfiguration};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MIN_CARGO_LLVM_COV_VERSION: &str = "0.7.0";
const ARM64_WINDOWS_TARGET: &str = "aarch64-pc-windows-msvc";
const TOOLCHAIN_ENV: &str = "COVERAGE_GATE_TOOLCHAIN";

pub(crate) fn run(args: &CoverageGateArgs, collection: &CollectionArgs) -> Result<ExitCode, AppError> {
    if !args.lcov.is_empty() {
        return Err(AppError::new(
            "`--lcov` cannot be used with `cargo coverage-gate run`; collected LCOV paths are selected by `--coverage-dir`",
        ));
    }
    let toolchain = ToolchainSelection::resolve(collection.toolchain.as_deref())?;
    let workspace = WorkspaceInfo::load(&toolchain)?;
    let selection = Selection::resolve(&workspace, &args.packages, collection.package_file.as_deref())?;
    if selection.explicit && selection.members.is_empty() {
        eprintln!("coverage-gate: package selection is empty; nothing to do");
        return Ok(ExitCode::SUCCESS);
    }

    let mut collection = collection.clone();
    collection.coverage_dir = absolute_path(&collection.coverage_dir)?;
    let configurations = normalized_configurations(&collection.configurations);

    if is_unsupported_arm64_windows_target(args.target.as_deref()) {
        let result =
            format!("`{ARM64_WINDOWS_TARGET}` does not support cargo-llvm-cov; tests passed without coverage collection or gating");
        run_plain_configurations(
            &workspace,
            &selection,
            &collection,
            &configurations,
            args.target.as_deref(),
            &toolchain,
            args.quiet,
        )?;
        crate::run::write_no_gate_summary(args, &result)?;
        eprintln!("coverage-gate: {result}");
        return Ok(ExitCode::SUCCESS);
    }

    let gated_names = selection.gated_names();
    let policy_probe = cargo_coverage_gate::evaluate_many_for_target_with_tools(
        &[],
        None,
        &gated_names,
        args.target.as_deref(),
        Some(toolchain.cargo()),
        Some(toolchain.rustc()),
        toolchain.name.as_deref(),
    )
    .into_app_err("failed to resolve coverage policy before collection")?;
    if !policy_probe.requires_coverage_collection() {
        let result = "every selected package has `min-lines-percent = 0`; tests passed without coverage collection or gating";
        run_plain_configurations(
            &workspace,
            &selection,
            &collection,
            &configurations,
            args.target.as_deref(),
            &toolchain,
            args.quiet,
        )?;
        crate::run::write_no_gate_summary(args, result)?;
        eprintln!("coverage-gate: {result}");
        return Ok(ExitCode::SUCCESS);
    }

    validate_instrumentation_toolchain(&workspace, &toolchain)?;
    fs::create_dir_all(&collection.coverage_dir).into_app_err(format!(
        "failed to create coverage directory `{}`",
        collection.coverage_dir.display()
    ))?;

    let tools = LlvmTools::discover(&toolchain)?;
    let coverage_target_dir = workspace.target_dir.join("llvm-cov-target");
    let execution = CollectionExecution {
        workspace: &workspace,
        selection: &selection,
        args: &collection,
        coverage_target_dir: &coverage_target_dir,
        tools: &tools,
        target: args.target.as_deref(),
        toolchain: &toolchain,
        quiet: args.quiet,
    };
    let mut lcov_paths = Vec::with_capacity(configurations.len());
    for configuration in configurations {
        let lcov_path = collect_configuration(&execution, configuration)?;
        lcov_paths.push(lcov_path);
    }

    crate::run::evaluate_paths_with_toolchain(
        args,
        &lcov_paths,
        &gated_names,
        Some(toolchain.cargo()),
        Some(toolchain.rustc()),
        toolchain.name.as_deref(),
    )
}

struct CollectionExecution<'a> {
    workspace: &'a WorkspaceInfo,
    selection: &'a Selection,
    args: &'a CollectionArgs,
    coverage_target_dir: &'a Path,
    tools: &'a LlvmTools,
    target: Option<&'a str>,
    toolchain: &'a ToolchainSelection,
    quiet: bool,
}

#[derive(Debug, Clone)]
struct ToolchainSelection {
    name: Option<OsString>,
    cargo: OsString,
    rustc: OsString,
}

impl ToolchainSelection {
    fn resolve(cli: Option<&str>) -> Result<Self, AppError> {
        let name = cli.map(OsString::from).or_else(|| env::var_os(TOOLCHAIN_ENV));
        if name.as_deref().is_some_and(OsStr::is_empty) {
            return Err(AppError::new(format!("`--toolchain` and `${TOOLCHAIN_ENV}` cannot be empty")));
        }
        let (cargo, rustc) = if let Some(name) = &name {
            let rustup = resolve_rustup()?;
            (rustup_which(&rustup, name, "cargo")?, rustup_which(&rustup, name, "rustc")?)
        } else {
            (
                env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")),
                env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc")),
            )
        };
        Ok(Self { name, cargo, rustc })
    }

    fn cargo(&self) -> &OsStr {
        &self.cargo
    }

    fn rustc(&self) -> &OsStr {
        &self.rustc
    }

    fn apply_to_command(&self, command: &mut Command) {
        if let Some(name) = &self.name {
            command.env("RUSTUP_TOOLCHAIN", name).env("RUSTC", &self.rustc);
        }
    }

    fn apply_to_metadata(&self, command: &mut MetadataCommand) {
        if let Some(name) = &self.name {
            command.env("RUSTUP_TOOLCHAIN", name);
            command.env("RUSTC", &self.rustc);
        }
    }
}

fn resolve_rustup() -> Result<PathBuf, AppError> {
    let current_dir = env::current_dir().into_app_err("failed to resolve the current directory while locating rustup")?;
    resolve_executable(
        OsStr::new("rustup"),
        env::var_os("RUSTUP").as_deref(),
        env::var_os("PATH").as_deref(),
        env::var_os("PATHEXT").as_deref(),
        &current_dir,
        cfg!(windows),
    )
}

fn resolve_executable(
    program: &OsStr,
    explicit: Option<&OsStr>,
    path: Option<&OsStr>,
    path_ext: Option<&OsStr>,
    current_dir: &Path,
    windows: bool,
) -> Result<PathBuf, AppError> {
    if let Some(explicit) = explicit {
        if explicit.is_empty() {
            return Err(AppError::new("`RUSTUP` cannot be empty"));
        }
        let explicit = PathBuf::from(explicit);
        if !explicit.is_absolute() {
            return Err(AppError::new(format!(
                "`RUSTUP` must be an absolute executable path, got `{}`",
                explicit.display()
            )));
        }
        if !is_executable_file(&explicit, windows) {
            return Err(AppError::new(format!(
                "`RUSTUP` does not name an executable file: `{}`",
                explicit.display()
            )));
        }
        return Ok(explicit);
    }

    let path = path.ok_or_else(|| AppError::new("cannot locate rustup because `PATH` is not set"))?;
    let names = executable_names(program, path_ext, windows);
    for directory in env::split_paths(path).filter(|directory| !directory.as_os_str().is_empty()) {
        let directory = if directory.is_absolute() {
            directory
        } else {
            current_dir.join(directory)
        };
        for name in &names {
            let candidate = directory.join(name);
            if is_executable_file(&candidate, windows) {
                return Ok(candidate);
            }
        }
    }
    Err(AppError::new(format!(
        "could not resolve `{}` from explicit non-empty `PATH` entries",
        program.to_string_lossy()
    )))
}

fn is_executable_file(path: &Path, windows: bool) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    platform_permissions_allow(&metadata, windows)
}

#[cfg(unix)]
// Windows-host mutation runs cannot compile this Unix-only permission branch;
// direct unit tests cover its arithmetic and Unix CI exercises the real mode.
#[mutants::skip]
fn platform_permissions_allow(metadata: &fs::Metadata, windows_semantics: bool) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    windows_semantics || has_executable_mode(metadata.permissions().mode())
}

#[cfg(not(unix))]
fn platform_permissions_allow(_metadata: &fs::Metadata, _windows_semantics: bool) -> bool {
    true
}

fn executable_names(program: &OsStr, path_ext: Option<&OsStr>, windows: bool) -> Vec<OsString> {
    if !windows || Path::new(program).extension().is_some() {
        return vec![program.to_os_string()];
    }
    let extensions = path_ext
        .and_then(OsStr::to_str)
        .filter(|extensions| !extensions.is_empty())
        .unwrap_or(".COM;.EXE;.BAT;.CMD");
    extensions
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| {
            let mut name = program.to_os_string();
            if !extension.starts_with('.') {
                name.push(".");
            }
            name.push(extension);
            name
        })
        .collect()
}

fn rustup_which(rustup: &Path, toolchain: &OsStr, program: &str) -> Result<OsString, AppError> {
    let display = format!("{} which --toolchain {} {program}", rustup.display(), toolchain.to_string_lossy());
    let output = Command::new(rustup)
        .args(["which", "--toolchain"])
        .arg(toolchain)
        .arg(program)
        .stderr(Stdio::inherit())
        .output()
        .into_app_err(format!("failed to execute `{display}`"))?;
    if !output.status.success() {
        return Err(AppError::new(format!("`{display}` exited with {}", output.status)));
    }
    let path = String::from_utf8(output.stdout).into_app_err(format!("`{display}` output was not UTF-8"))?;
    let path = path.trim();
    if path.is_empty() {
        return Err(AppError::new(format!("`{display}` did not report a program path")));
    }
    Ok(OsString::from(path))
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
    fn load(toolchain: &ToolchainSelection) -> Result<Self, AppError> {
        let mut command = MetadataCommand::new();
        command.no_deps().cargo_path(toolchain.cargo());
        toolchain.apply_to_metadata(&mut command);
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
    fn resolve(workspace: &WorkspaceInfo, package_selectors: &[String], package_file: Option<&Path>) -> Result<Self, AppError> {
        let file_specs = package_file.map(read_package_file).transpose()?.unwrap_or_default();
        let explicit = package_file.is_some() || !package_selectors.is_empty();
        if !explicit {
            return Ok(Self {
                explicit,
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
        for spec in file_specs {
            let Some(member) = workspace.members.iter().find(|member| member.spec() == spec) else {
                return Err(AppError::new(format!(
                    "package file entry `{spec}` did not match an exact `name@version` workspace member"
                )));
            };
            selected.insert(member.clone());
        }

        Ok(Self {
            explicit,
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

fn read_package_file(path: &Path) -> Result<Vec<String>, AppError> {
    let contents = fs::read_to_string(path).into_app_err(format!("failed to read package file `{}` as UTF-8", path.display()))?;
    Ok(contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
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
            Self::NoDefaultFeatures => "no-default-features",
        }
    }
}

fn is_unsupported_arm64_windows_target(target: Option<&str>) -> bool {
    let processor = env::var("PROCESSOR_ARCHITECTURE").ok();
    let wow64_processor = env::var("PROCESSOR_ARCHITEW6432").ok();
    is_arm64_windows_target(target, cfg!(windows), processor.as_deref(), wow64_processor.as_deref())
}

fn is_arm64_windows_target(
    target: Option<&str>,
    host_is_windows: bool,
    processor_architecture: Option<&str>,
    wow64_processor_architecture: Option<&str>,
) -> bool {
    match target {
        Some(target) => target == ARM64_WINDOWS_TARGET,
        None => {
            host_is_windows
                && [processor_architecture, wow64_processor_architecture]
                    .into_iter()
                    .flatten()
                    .any(|architecture| architecture.eq_ignore_ascii_case("ARM64"))
        }
    }
}

fn validate_instrumentation_toolchain(workspace: &WorkspaceInfo, toolchain: &ToolchainSelection) -> Result<(), AppError> {
    let mut cargo_version = cargo_command(workspace, toolchain);
    cargo_version.args(["--version", "--verbose"]);
    let cargo_version = read_stdout(&mut cargo_version, "cargo toolchain validation")?;
    let cargo_release =
        cargo_release(&cargo_version).ok_or_else(|| AppError::new("`cargo --version --verbose` did not report a release"))?;
    if !cargo_release.contains("-nightly") {
        return Err(AppError::new(format!(
            "`cargo coverage-gate run` requires a nightly Rust toolchain for `cfg(coverage_nightly)`, but selected Cargo release `{cargo_release}`; use `--toolchain <nightly>` or `${TOOLCHAIN_ENV}`"
        )));
    }

    let mut rustc_version = Command::new(toolchain.rustc());
    rustc_version.arg("-vV");
    toolchain.apply_to_command(&mut rustc_version);
    let rustc_version = read_stdout(&mut rustc_version, "rustc toolchain validation")?;
    let rustc_release = rustc_release(&rustc_version).ok_or_else(|| AppError::new("`rustc -vV` did not report a release"))?;
    if !rustc_release.contains("-nightly") {
        return Err(AppError::new(format!(
            "`cargo coverage-gate run` requires nightly rustc for `cfg(coverage_nightly)`, but selected rustc release `{rustc_release}`; use `--toolchain <nightly>` or `${TOOLCHAIN_ENV}`"
        )));
    }

    let mut llvm_cov_version = cargo_command(workspace, toolchain);
    llvm_cov_version.args(["llvm-cov", "--version"]);
    let llvm_cov_version = read_stdout(&mut llvm_cov_version, "cargo-llvm-cov version validation")?;
    let version = cargo_llvm_cov_version(&llvm_cov_version)?;
    let minimum =
        Version::parse(MIN_CARGO_LLVM_COV_VERSION).expect("MIN_CARGO_LLVM_COV_VERSION is a compile-time semantic version literal");
    if !cargo_llvm_cov_is_supported(&version, &minimum) {
        return Err(AppError::new(format!(
            "`cargo coverage-gate run` requires cargo-llvm-cov >= {minimum}, but selected {version}"
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
    toolchain: &ToolchainSelection,
    quiet: bool,
) -> Result<(), AppError> {
    for &configuration in configurations {
        let mut command = cargo_command(workspace, toolchain);
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
    let final_lcov = execution
        .args
        .coverage_dir
        .join(format!("lcov-{}.info", configuration.artifact_name()));

    run_clean(
        execution.workspace,
        execution.coverage_target_dir,
        execution.toolchain,
        execution.quiet,
    )?;
    let objects = run_nextest(execution, configuration)?;
    if objects.is_empty() {
        return Err(AppError::new(format!(
            "cargo-llvm-cov produced no executable object paths for the `{}` configuration",
            configuration.artifact_name()
        )));
    }

    let profraw_files = discover_profraw_files(execution.coverage_target_dir)?;
    if profraw_files.is_empty() {
        return Err(AppError::new(format!(
            "cargo-llvm-cov produced no raw profiles for the `{}` configuration",
            configuration.artifact_name()
        )));
    }

    let profile_list = TemporaryPath::write_atomic(
        &execution.args.coverage_dir,
        &format!("{}-profraw-list", configuration.artifact_name()),
        &profile_list_contents(&profraw_files)?,
    )?;
    let profdata = TemporaryPath::new(&execution.args.coverage_dir, &format!("{}.profdata", configuration.artifact_name()));
    run_profdata_merge(execution.tools, profile_list.path(), profdata.path(), execution.quiet)?;

    let response = TemporaryPath::write_atomic(
        &execution.args.coverage_dir,
        &format!("{}-objects.rsp", configuration.artifact_name()),
        &object_response_contents(&objects)?,
    )?;
    export_lcov(
        execution.workspace,
        execution.tools,
        profdata.path(),
        response.path(),
        &final_lcov,
        &execution.args.coverage_dir,
        configuration,
    )?;

    response.cleanup()?;
    profile_list.cleanup()?;
    profdata.cleanup()?;
    Ok(final_lcov)
}

fn cargo_command(workspace: &WorkspaceInfo, toolchain: &ToolchainSelection) -> Command {
    let mut command = Command::new(toolchain.cargo());
    command.current_dir(&workspace.root);
    toolchain.apply_to_command(&mut command);
    command
}

fn coverage_command(workspace: &WorkspaceInfo, coverage_target_dir: &Path, toolchain: &ToolchainSelection) -> Command {
    let mut command = cargo_command(workspace, toolchain);
    command
        .env("CARGO_LLVM_COV_TARGET_DIR", coverage_target_dir)
        .env("CARGO_LLVM_COV_BUILD_DIR", coverage_target_dir);
    command
}

fn run_clean(workspace: &WorkspaceInfo, coverage_target_dir: &Path, toolchain: &ToolchainSelection, quiet: bool) -> Result<(), AppError> {
    let mut command = coverage_command(workspace, coverage_target_dir, toolchain);
    command.args(["llvm-cov", "clean", "--workspace"]);
    if quiet {
        command.stdout(Stdio::null());
    }
    run_status(&mut command, "cargo llvm-cov clean")
}

fn run_nextest(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration) -> Result<Vec<PathBuf>, AppError> {
    let mut command = coverage_command(execution.workspace, execution.coverage_target_dir, execution.toolchain);
    command.args(["llvm-cov", "nextest", "--no-report"]);
    append_package_selection(&mut command, execution.selection);
    append_nextest_options(&mut command, execution.args, configuration, execution.target);
    command.arg("--cargo-message-format=json-render-diagnostics");
    command.stdout(Stdio::piped());

    let display = command_display(&command);
    let mut child = command.spawn().into_app_err(format!("failed to execute `{display}`"))?;
    let stdout = child
        .stdout
        .take()
        .expect("stdout was configured as piped immediately before spawn");
    let mut objects = BTreeSet::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.into_app_err(format!("failed to read output from `{display}`"))?;
        match compiler_artifact_objects(&line) {
            Ok(artifact_objects) => objects.extend(artifact_objects),
            Err(_error) if !execution.quiet => println!("{line}"),
            Err(_error) => {}
        }
    }
    let status = child.wait().into_app_err(format!("failed to wait for `{display}`"))?;
    if !status.success() {
        return Err(AppError::new(format!("`{display}` exited with {status}")));
    }
    Ok(objects.into_iter().collect())
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
    command.arg(configuration.cargo_flag());
    if let Some(target) = target {
        command.arg("--target").arg(target);
    }
    if let Some(jobs) = args.jobs {
        command.arg("--jobs").arg(jobs.get().to_string());
        command.arg("--build-jobs").arg(jobs.get().to_string());
    }
}

fn compiler_artifact_objects(line: &str) -> serde_json::Result<Vec<PathBuf>> {
    let message: Value = serde_json::from_str(line)?;
    if message.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
        return Ok(Vec::new());
    }
    if message
        .get("target")
        .and_then(|target| target.get("kind"))
        .and_then(Value::as_array)
        .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("custom-build")))
    {
        return Ok(Vec::new());
    }

    let mut objects = BTreeSet::new();
    if let Some(executable) = message.get("executable").and_then(Value::as_str) {
        objects.insert(PathBuf::from(executable));
    }
    if let Some(filenames) = message.get("filenames").and_then(Value::as_array) {
        objects.extend(
            filenames
                .iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .filter(|path| is_coverage_object(path)),
        );
    }
    Ok(objects.into_iter().collect())
}

fn is_coverage_object(path: &Path) -> bool {
    if is_ignored_artifact_path(path) {
        return false;
    }
    #[cfg(windows)]
    {
        let extension = path.extension().unwrap_or_default();
        if !(extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("dll")) {
            return false;
        }
    }
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        has_executable_mode(metadata.permissions().mode())
    }
    #[cfg(not(unix))]
    true
}

fn is_ignored_artifact_path(path: &Path) -> bool {
    let extension = path.extension().unwrap_or_default();
    extension == "d" || extension == "rlib" || extension == "rmeta" || path.ends_with(".cargo-lock") || path.ends_with(".cargo-build-lock")
}

#[cfg(any(unix, test))]
fn has_executable_mode(mode: u32) -> bool {
    mode & 0o111 != 0
}

fn discover_profraw_files(target_dir: &Path) -> Result<Vec<PathBuf>, AppError> {
    let entries =
        fs::read_dir(target_dir).into_app_err(format!("failed to inspect coverage target directory `{}`", target_dir.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .into_app_err(format!("failed to inspect coverage target directory `{}`", target_dir.display()))?
            .path();
        if path.extension() == Some(OsStr::new("profraw")) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn profile_list_contents(paths: &[PathBuf]) -> Result<Vec<u8>, AppError> {
    let mut output = Vec::new();
    for path in paths {
        let Some(path) = path.to_str() else {
            return Err(AppError::new(format!("raw profile path `{}` is not valid UTF-8", path.display())));
        };
        if path.contains(['\n', '\r']) {
            return Err(AppError::new(format!(
                "raw profile path `{path}` contains a line break and cannot be written to the llvm-profdata input list"
            )));
        }
        output.extend_from_slice(path.as_bytes());
        output.push(b'\n');
    }
    Ok(output)
}

fn object_response_contents(objects: &[PathBuf]) -> Result<Vec<u8>, AppError> {
    let mut output = Vec::new();
    for object in objects {
        let Some(object) = object.to_str() else {
            return Err(AppError::new(format!("object path `{}` is not valid UTF-8", object.display())));
        };
        output.extend_from_slice(b"-object\n");
        output.extend_from_slice(quote_response_argument(object)?.as_bytes());
        output.push(b'\n');
    }
    Ok(output)
}

fn quote_response_argument(argument: &str) -> Result<String, AppError> {
    if argument.contains(['\0', '\n', '\r']) {
        return Err(AppError::new(
            "LLVM response-file arguments cannot contain NUL or newline characters",
        ));
    }
    #[cfg(windows)]
    let argument = argument.replace('\\', "/");
    #[cfg(not(windows))]
    let argument = argument.to_owned();
    Ok(format!("\"{}\"", argument.replace('\\', "\\\\").replace('"', "\\\"")))
}

fn run_profdata_merge(tools: &LlvmTools, profile_list: &Path, output: &Path, quiet: bool) -> Result<(), AppError> {
    let mut command = Command::new(&tools.profdata);
    command.args(["merge", "-sparse", "-f"]).arg(profile_list).arg("-o").arg(output);
    append_space_separated_env(&mut command, "LLVM_PROFDATA_FLAGS");
    if quiet {
        command.stdout(Stdio::null());
    }
    run_status(&mut command, "llvm-profdata merge")
}

fn export_lcov(
    workspace: &WorkspaceInfo,
    tools: &LlvmTools,
    profdata: &Path,
    response: &Path,
    final_lcov: &Path,
    coverage_dir: &Path,
    configuration: FeatureConfiguration,
) -> Result<(), AppError> {
    let temporary_lcov = TemporaryPath::new(coverage_dir, &format!("{}.lcov", configuration.artifact_name()));
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary_lcov.path())
        .into_app_err(format!(
            "failed to create temporary LCOV file `{}`",
            temporary_lcov.path().display()
        ))?;

    let mut command = Command::new(&tools.cov);
    command
        .current_dir(&workspace.root)
        .arg("export")
        .arg("-format=lcov")
        .arg(format!("-instr-profile={}", profdata.display()))
        .arg(format!("@{}", response.display()))
        .arg("-ignore-filename-regex")
        .arg(default_ignore_filename_regex(workspace));
    append_space_separated_env(&mut command, "LLVM_COV_FLAGS");
    command.stdout(Stdio::from(output));
    run_status(&mut command, "llvm-cov export")?;

    replace_file_atomically(temporary_lcov.path(), final_lcov)
        .into_app_err(format!("failed to publish LCOV file `{}`", final_lcov.display()))?;
    temporary_lcov.disarm();
    Ok(())
}

#[cfg(not(windows))]
// This branch is not compiled by Windows-host mutation runs. Its behavior is
// covered by the same replacement tests on Unix CI.
#[mutants::skip]
fn replace_file_atomically(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file_atomically(source: &Path, destination: &Path) -> io::Result<()> {
    use std::iter;
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let source = source.as_os_str().encode_wide().chain(iter::once(0)).collect::<Vec<_>>();
    let destination = destination.as_os_str().encode_wide().chain(iter::once(0)).collect::<Vec<_>>();
    // SAFETY: both pointers reference live, NUL-terminated UTF-16 buffers for
    // the duration of the call. The files are in the same directory, and the
    // flags request an atomic replacement without exposing an absent final
    // path.
    let replaced = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), windows_replace_flags()) };
    if replaced == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(windows)]
// MOVEFILE_REPLACE_EXISTING and MOVEFILE_WRITE_THROUGH are disjoint bits, so
// cargo-mutants' `|` to `^` mutation is behaviorally equivalent.
#[mutants::skip]
fn windows_replace_flags() -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
}
fn append_space_separated_env(command: &mut Command, key: &str) {
    if let Some(flags) = env::var_os(key) {
        command.args(flags.to_string_lossy().split(' ').filter(|flag| !flag.trim_start().is_empty()));
    }
}

fn default_ignore_filename_regex(workspace: &WorkspaceInfo) -> String {
    let separator = if cfg!(windows) { r"\\" } else { "/" };
    let root = regex_escape(&workspace.root.to_string_lossy());
    let target = regex_escape(&workspace.target_dir.to_string_lossy());
    format!(
        r"{separator}rustc{separator}([0-9a-f]+|[0-9]+\.[0-9]+\.[0-9]+){separator}|^{root}({separator}.*)?{separator}(tests|examples|benches){separator}|^{root}({separator}.*)?{separator}(tests\.rs|[0-9A-Za-z_-]+[_-]tests\.rs)$|^{target}($|{separator})"
    )
}

fn regex_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
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

#[derive(Debug)]
struct LlvmTools {
    cov: PathBuf,
    profdata: PathBuf,
}

impl LlvmTools {
    fn discover(toolchain: &ToolchainSelection) -> Result<Self, AppError> {
        let cov = env::var_os("LLVM_COV").map(PathBuf::from);
        let profdata = env::var_os("LLVM_PROFDATA").map(PathBuf::from);
        if let (Some(cov), Some(profdata)) = (cov.clone(), profdata.clone()) {
            return Ok(Self { cov, profdata });
        }

        let rustc = toolchain.rustc();
        let mut command = Command::new(rustc);
        command.args(["--print", "target-libdir"]);
        toolchain.apply_to_command(&mut command);
        let output = command
            .output()
            .into_app_err(format!("failed to execute `{}` to locate LLVM tools", Path::new(rustc).display()))?;
        if !output.status.success() {
            return Err(AppError::new(format!(
                "`{} --print target-libdir` exited with {}: {}",
                Path::new(rustc).display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let target_libdir = String::from_utf8(output.stdout).into_app_err("rustc target-libdir output was not UTF-8")?;
        let Some(rustlib) = Path::new(target_libdir.trim()).parent() else {
            return Err(AppError::new("rustc target-libdir output had no parent directory"));
        };
        let bin = rustlib.join("bin");
        Ok(Self {
            cov: cov.unwrap_or_else(|| bin.join(format!("llvm-cov{}", env::consts::EXE_SUFFIX))),
            profdata: profdata.unwrap_or_else(|| bin.join(format!("llvm-profdata{}", env::consts::EXE_SUFFIX))),
        })
    }
}

#[derive(Debug)]
struct TemporaryPath {
    path: PathBuf,
    armed: bool,
}

impl TemporaryPath {
    fn new(directory: &Path, label: &str) -> Self {
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: directory.join(format!(".coverage-gate-{}-{sequence}-{label}", std::process::id())),
            armed: true,
        }
    }

    fn write_atomic(directory: &Path, label: &str, contents: &[u8]) -> Result<Self, AppError> {
        let published = Self::new(directory, label);
        let staging = Self::new(directory, &format!("{label}.staging"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staging.path())
            .into_app_err(format!("failed to create temporary file `{}`", staging.path().display()))?;
        file.write_all(contents)
            .into_app_err(format!("failed to write temporary file `{}`", staging.path().display()))?;
        file.sync_all()
            .into_app_err(format!("failed to flush temporary file `{}`", staging.path().display()))?;
        drop(file);
        atomic_rename(&staging, &published)?;
        staging.disarm();
        Ok(published)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(mut self) {
        self.armed = false;
    }

    fn cleanup(mut self) -> Result<(), AppError> {
        remove_if_present(&self.path).into_app_err(format!("failed to remove temporary file `{}`", self.path.display()))?;
        self.armed = false;
        Ok(())
    }
}

fn atomic_rename(staging: &TemporaryPath, published: &TemporaryPath) -> Result<(), AppError> {
    fs::rename(staging.path(), published.path()).into_app_err(format!(
        "failed to atomically publish temporary file `{}`",
        published.path().display()
    ))
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    fn invalid_unicode_path() -> PathBuf {
        use std::os::unix::ffi::OsStringExt as _;

        OsString::from_vec(vec![0xFF]).into()
    }

    #[cfg(windows)]
    fn invalid_unicode_path() -> PathBuf {
        use std::os::windows::ffi::OsStringExt as _;

        OsString::from_wide(&[0xD800]).into()
    }

    fn member(name: &str, version: &str) -> WorkspaceMember {
        WorkspaceMember {
            name: name.to_owned(),
            version: version.to_owned(),
        }
    }

    #[test]
    fn default_configurations_include_both_supported_modes() {
        assert_eq!(
            normalized_configurations(&[]),
            [FeatureConfiguration::AllFeatures, FeatureConfiguration::NoDefaultFeatures]
        );
    }

    #[test]
    fn arm64_windows_detection_honors_explicit_target_and_native_architecture() {
        assert!(is_arm64_windows_target(Some(ARM64_WINDOWS_TARGET), false, None, None));
        assert!(!is_arm64_windows_target(Some("x86_64-pc-windows-msvc"), true, Some("ARM64"), None));
        assert!(is_arm64_windows_target(None, true, Some("ARM64"), None));
        assert!(is_arm64_windows_target(None, true, Some("AMD64"), Some("arm64")));
        assert!(!is_arm64_windows_target(None, false, Some("ARM64"), Some("ARM64")));
        assert!(!is_arm64_windows_target(None, true, Some("AMD64"), None));
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
            cargo_llvm_cov_version("cargo-llvm-cov 0.9.0\n").expect("valid version"),
            Version::new(0, 9, 0)
        );
        let minimum = Version::new(0, 7, 0);
        assert!(!cargo_llvm_cov_is_supported(&Version::new(0, 6, 9), &minimum));
        assert!(cargo_llvm_cov_is_supported(&Version::new(0, 7, 0), &minimum));
        assert!(cargo_llvm_cov_is_supported(&Version::new(0, 8, 0), &minimum));
        cargo_llvm_cov_version("cargo llvm-cov 0.9.0\n").expect_err("missing package version prefix must fail");
        cargo_llvm_cov_version("cargo-llvm-cov development\n").expect_err("non-semver version must fail");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary executable files, which Miri isolation does not support")]
    fn executable_resolution_ignores_planted_cwd_files_unless_cwd_is_in_path() {
        let tmp = tempdir().expect("tempdir");
        let current_dir = tmp.path().join("project");
        let trusted = tmp.path().join("trusted");
        fs::create_dir_all(&current_dir).expect("create project");
        fs::create_dir_all(&trusted).expect("create trusted directory");
        let planted = current_dir.join("rustup.EXE");
        let trusted_rustup = trusted.join("rustup.EXE");
        fs::write(&planted, b"planted").expect("write planted rustup");
        fs::write(&trusted_rustup, b"trusted").expect("write trusted rustup");

        let path = env::join_paths([trusted.clone()]).expect("trusted PATH");
        assert_eq!(
            resolve_executable(
                OsStr::new("rustup"),
                None,
                Some(&path),
                Some(OsStr::new(".EXE")),
                &current_dir,
                true
            )
            .expect("resolve trusted rustup"),
            trusted_rustup
        );

        let path_with_empty = env::join_paths([PathBuf::new(), trusted.clone()]).expect("PATH with an empty entry");
        assert_eq!(
            resolve_executable(
                OsStr::new("rustup"),
                None,
                Some(&path_with_empty),
                Some(OsStr::new(".EXE")),
                &current_dir,
                true,
            )
            .expect("empty PATH entry must be ignored"),
            trusted_rustup
        );

        let path_with_explicit_cwd = env::join_paths([PathBuf::from("."), trusted]).expect("PATH with explicit current directory");
        assert_eq!(
            resolve_executable(
                OsStr::new("rustup"),
                None,
                Some(&path_with_explicit_cwd),
                Some(OsStr::new(".EXE")),
                &current_dir,
                true,
            )
            .expect("explicit current directory must be honored"),
            planted
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary executable files, which Miri isolation does not support")]
    fn windows_executable_resolution_honors_pathext() {
        let tmp = tempdir().expect("tempdir");
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).expect("create bin");
        let rustup = bin.join("rustup.RUSTUPTEST");
        fs::write(&rustup, b"rustup").expect("write PATHEXT rustup");
        let path = env::join_paths([bin]).expect("bin PATH");

        assert_eq!(
            resolve_executable(
                OsStr::new("rustup"),
                None,
                Some(&path),
                Some(OsStr::new(".RUSTUPTEST;.EXE")),
                tmp.path(),
                true,
            )
            .expect("resolve PATHEXT executable"),
            rustup
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary executable paths, which Miri isolation does not support")]
    fn executable_resolution_rejects_missing_and_non_file_candidates() {
        let tmp = tempdir().expect("tempdir");
        resolve_executable(OsStr::new("rustup"), None, None, None, tmp.path(), true).expect_err("missing PATH must fail resolution");
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).expect("create bin");
        fs::create_dir(bin.join("rustup.EXE")).expect("create directory shaped like an executable");
        let path = env::join_paths([bin]).expect("bin PATH");

        resolve_executable(OsStr::new("rustup"), None, Some(&path), Some(OsStr::new(".EXE")), tmp.path(), true)
            .expect_err("a directory must not resolve as an executable");
    }

    #[test]
    fn executable_name_expansion_covers_platform_and_pathext_forms() {
        assert_eq!(executable_names(OsStr::new("rustup"), None, false), [OsString::from("rustup")]);
        assert_eq!(
            executable_names(OsStr::new("rustup.exe"), Some(OsStr::new(".CUSTOM")), true),
            [OsString::from("rustup.exe")]
        );
        assert_eq!(
            executable_names(OsStr::new("rustup"), Some(OsStr::new("CUSTOM")), true),
            [OsString::from("rustup.CUSTOM")]
        );
        let defaults = executable_names(OsStr::new("rustup"), None, true);
        assert_eq!(
            defaults,
            [
                OsString::from("rustup.COM"),
                OsString::from("rustup.EXE"),
                OsString::from("rustup.BAT"),
                OsString::from("rustup.CMD"),
            ]
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary executable files, which Miri isolation does not support")]
    fn explicit_rustup_override_must_be_nonempty_absolute_and_executable() {
        let tmp = tempdir().expect("tempdir");
        let rustup = tmp.path().join("rustup.exe");
        fs::write(&rustup, b"rustup").expect("write rustup");

        resolve_executable(OsStr::new("rustup"), Some(OsStr::new("")), None, None, tmp.path(), true).expect_err("empty override must fail");
        resolve_executable(OsStr::new("rustup"), Some(OsStr::new("rustup.exe")), None, None, tmp.path(), true)
            .expect_err("relative override must fail");
        let missing = tmp.path().join("missing.exe");
        resolve_executable(OsStr::new("rustup"), Some(missing.as_os_str()), None, None, tmp.path(), true)
            .expect_err("missing override must fail");
        assert_eq!(
            resolve_executable(OsStr::new("rustup"), Some(rustup.as_os_str()), None, None, tmp.path(), true,)
                .expect("absolute executable override"),
            rustup
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_atomic_replace_uses_replace_and_write_through_flags() {
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

        assert_eq!(windows_replace_flags(), MOVEFILE_REPLACE_EXISTING + MOVEFILE_WRITE_THROUGH);
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
    fn compiler_artifact_parser_returns_executable_field() {
        assert_eq!(
            compiler_artifact_objects(r#"{"reason":"compiler-artifact","executable":"target/debug/deps/alpha"}"#).expect("Cargo JSON"),
            [PathBuf::from("target/debug/deps/alpha")]
        );
        assert!(
            compiler_artifact_objects(r#"{"reason":"compiler-artifact","executable":null}"#)
                .expect("Cargo JSON")
                .is_empty()
        );
        assert!(
            compiler_artifact_objects(r#"{"reason":"build-finished","success":true}"#)
                .expect("Cargo JSON")
                .is_empty()
        );
        compiler_artifact_objects("not json").expect_err("non-JSON output must be rejected");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "uses temporary files and filesystem metadata, which Miri isolation does not support"
    )]
    fn compiler_artifact_parser_includes_object_filenames_and_excludes_build_scripts() {
        let tmp = tempdir().expect("tempdir");
        let object = if cfg!(windows) {
            tmp.path().join("proc_macro.dll")
        } else {
            tmp.path().join("proc_macro")
        };
        fs::write(&object, b"object").expect("write object");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let mut permissions = fs::metadata(&object).expect("object metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&object, permissions).expect("mark object executable");
        }
        let rlib = tmp.path().join("library.rlib");
        fs::write(&rlib, b"rlib").expect("write rlib");
        let message = serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "kind": ["proc-macro"] },
            "filenames": [object, rlib],
            "executable": null
        })
        .to_string();
        let objects = compiler_artifact_objects(&message).expect("Cargo JSON");
        assert_eq!(objects.as_slice(), std::slice::from_ref(&object));

        let build_script = serde_json::json!({
            "reason": "compiler-artifact",
            "target": { "kind": ["custom-build"] },
            "filenames": [object],
            "executable": "build-script"
        })
        .to_string();
        assert!(compiler_artifact_objects(&build_script).expect("Cargo JSON").is_empty());
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "uses temporary files and filesystem metadata, which Miri isolation does not support"
    )]
    fn object_filter_rejects_non_objects() {
        let tmp = tempdir().expect("tempdir");
        #[cfg(windows)]
        {
            let wrong_extension = tmp.path().join("object.txt");
            fs::write(&wrong_extension, b"object").expect("write non-object");
            assert!(!is_coverage_object(&wrong_extension));
        }

        let missing = if cfg!(windows) {
            tmp.path().join("missing.dll")
        } else {
            tmp.path().join("missing")
        };
        assert!(!is_coverage_object(&missing));

        let directory = if cfg!(windows) {
            tmp.path().join("directory.dll")
        } else {
            tmp.path().join("directory")
        };
        fs::create_dir(&directory).expect("create object-shaped directory");
        assert!(!is_coverage_object(&directory));
    }

    #[test]
    fn ignored_artifact_names_cover_each_exclusion() {
        for ignored in ["artifact.d", "artifact.rlib", "artifact.rmeta", ".cargo-lock", ".cargo-build-lock"] {
            assert!(is_ignored_artifact_path(Path::new(ignored)), "{ignored}");
        }
        assert!(!is_ignored_artifact_path(Path::new("artifact")));
        assert!(!is_ignored_artifact_path(Path::new("artifact.exe")));
    }

    #[test]
    fn executable_mode_accepts_each_execute_bit() {
        assert!(has_executable_mode(0o100));
        assert!(has_executable_mode(0o010));
        assert!(has_executable_mode(0o001));
        assert!(!has_executable_mode(0));
        assert!(!has_executable_mode(0o600));
    }

    #[test]
    fn response_file_places_each_object_behind_object_flag() {
        let contents = object_response_contents(&[PathBuf::from("target/one"), PathBuf::from("target/object with spaces")])
            .expect("response contents");
        let contents = String::from_utf8(contents).expect("UTF-8 response");
        assert_eq!(contents.matches("-object\n").count(), 2);
        assert!(contents.contains("\"target/one\""));
        assert!(contents.contains("\"target/object with spaces\""));
    }

    #[test]
    fn response_argument_rejects_line_breaks() {
        quote_response_argument("one\nobject").expect_err("line feeds must be rejected");
        quote_response_argument("one\robject").expect_err("carriage returns must be rejected");
        quote_response_argument("one\0object").expect_err("NUL characters must be rejected");
    }

    #[test]
    fn regex_escape_covers_every_metacharacter_used_by_paths() {
        assert_eq!(regex_escape(r"a.b[c]\d+$"), r"a\.b\[c\]\\d\+\$");
        assert_eq!(regex_escape("plain/path"), "plain/path");
    }

    #[test]
    fn default_ignore_regex_names_workspace_tests_and_target_output() {
        #[cfg(windows)]
        let root = PathBuf::from(r"C:\workspace");
        #[cfg(not(windows))]
        let root = PathBuf::from("/workspace");
        let workspace = WorkspaceInfo {
            target_dir: root.join("target"),
            root,
            members: Vec::new(),
        };

        let regex = default_ignore_filename_regex(&workspace);
        assert!(regex.contains("rustc"));
        assert!(regex.contains("tests|examples|benches"));
        assert!(regex.contains(&regex_escape(&workspace.root.to_string_lossy())));
        assert!(regex.contains(&regex_escape(&workspace.target_dir.to_string_lossy())));
    }

    #[test]
    fn profile_and_object_lists_reject_non_utf8_paths() {
        let path = invalid_unicode_path();
        profile_list_contents(std::slice::from_ref(&path)).expect_err("profile paths must be UTF-8");
        object_response_contents(&[path]).expect_err("object paths must be UTF-8");
    }

    #[test]
    fn profile_lists_reject_carriage_returns_and_line_feeds() {
        for path in ["target/bad\nprofile.profraw", "target/bad\rprofile.profraw"] {
            let error = profile_list_contents(&[PathBuf::from(path)]).expect_err("line-delimited profile paths must reject CR/LF");
            assert!(error.to_string().contains("line break"), "{error}");
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn export_reports_an_unusable_temporary_directory() {
        let tmp = tempdir().expect("tempdir");
        let not_a_directory = tmp.path().join("not-a-directory");
        fs::write(&not_a_directory, b"file").expect("write conflicting file");
        let workspace = WorkspaceInfo {
            root: tmp.path().to_path_buf(),
            target_dir: tmp.path().join("target"),
            members: Vec::new(),
        };
        let tools = LlvmTools {
            cov: PathBuf::from("unused-llvm-cov"),
            profdata: PathBuf::from("unused-llvm-profdata"),
        };

        export_lcov(
            &workspace,
            &tools,
            Path::new("unused.profdata"),
            Path::new("unused.rsp"),
            &tmp.path().join("unused.info"),
            &not_a_directory,
            FeatureConfiguration::AllFeatures,
        )
        .expect_err("temporary LCOV creation must fail before spawning llvm-cov");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn remove_if_present_reports_a_directory() {
        let tmp = tempdir().expect("tempdir");
        let directory = tmp.path().join("directory");
        fs::create_dir(&directory).expect("create directory");

        remove_if_present(&directory).expect_err("remove_file must reject a directory");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files and filesystem rename, which Miri isolation does not support")]
    fn atomic_rename_reports_a_missing_staging_file() {
        let tmp = tempdir().expect("tempdir");
        let staging = TemporaryPath::new(tmp.path(), "missing");
        let published = TemporaryPath::new(tmp.path(), "published");

        atomic_rename(&staging, &published).expect_err("a missing staging file must fail publication");
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "uses temporary files and atomic replacement, which Miri isolation does not support"
    )]
    fn atomic_replacement_replaces_existing_bytes() {
        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("new.info");
        let destination = tmp.path().join("final.info");
        fs::write(&source, b"new").expect("write replacement");
        fs::write(&destination, b"old").expect("write prior artifact");

        replace_file_atomically(&source, &destination).expect("replace existing file");

        assert_eq!(fs::read(&destination).expect("read replacement"), b"new");
        assert!(!source.exists());
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "uses temporary files and atomic replacement, which Miri isolation does not support"
    )]
    fn failed_atomic_replacement_preserves_existing_bytes() {
        let tmp = tempdir().expect("tempdir");
        let source = tmp.path().join("missing.info");
        let destination = tmp.path().join("final.info");
        fs::write(&destination, b"old").expect("write prior artifact");

        replace_file_atomically(&source, &destination).expect_err("missing replacement must fail");

        assert_eq!(fs::read(destination).expect("read preserved artifact"), b"old");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn disarm_preserves_the_temporary_file() {
        let tmp = tempdir().expect("tempdir");
        let temporary = TemporaryPath::new(tmp.path(), "preserved");
        let path = temporary.path().to_path_buf();
        fs::write(&path, b"complete").expect("write temporary file");

        temporary.disarm();

        assert_eq!(fs::read(path).expect("read disarmed file"), b"complete");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn cleanup_removes_the_temporary_file() {
        let tmp = tempdir().expect("tempdir");
        let temporary = TemporaryPath::new(tmp.path(), "removed");
        let path = temporary.path().to_path_buf();
        fs::write(&path, b"temporary").expect("write temporary file");

        temporary.cleanup().expect("clean temporary file");

        assert!(!path.exists());
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary directories, which Miri isolation does not support")]
    fn cleanup_reports_an_unremovable_temporary_path() {
        let tmp = tempdir().expect("tempdir");
        let temporary = TemporaryPath::new(tmp.path(), "directory");
        fs::create_dir(temporary.path()).expect("create directory at temporary path");

        temporary
            .cleanup()
            .expect_err("explicit cleanup must report that a directory cannot be removed as a file");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn remove_if_present_accepts_a_missing_file() {
        let tmp = tempdir().expect("tempdir");
        remove_if_present(&tmp.path().join("missing")).expect("a missing file is already removed");
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
