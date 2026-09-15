// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Portable coverage collection for `cargo coverage-gate run`.

use std::collections::BTreeSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use cargo_metadata::{Metadata, MetadataCommand};
use ohno::{AppError, EnrichableExt as _, IntoAppError};
use semver::Version;

use crate::cli::{CollectionArgs, CoverageGateArgs, FeatureConfiguration};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MIN_CARGO_LLVM_COV_VERSION: &str = "0.8.0";
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

    if is_unsupported_arm64_windows_target(args.target.as_deref(), &toolchain)? {
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

    let coverage_scratch = TemporaryDirectory::create(&workspace.target_dir.join("coverage-gate"))?;
    let coverage_target_dir = coverage_scratch.path().join("cargo-target");
    let execution = CollectionExecution {
        workspace: &workspace,
        selection: &selection,
        args: &collection,
        scratch_dir: coverage_scratch.path(),
        coverage_target_dir: &coverage_target_dir,
        target: args.target.as_deref(),
        toolchain: &toolchain,
        quiet: args.quiet,
    };
    let mut lcov_paths = Vec::with_capacity(configurations.len());
    for configuration in configurations {
        let lcov_path = collect_configuration(&execution, configuration)?;
        lcov_paths.push(lcov_path);
    }

    let evaluation = crate::run::evaluate_paths_with_toolchain(
        args,
        &lcov_paths,
        &gated_names,
        Some(toolchain.cargo()),
        Some(toolchain.rustc()),
        toolchain.name.as_deref(),
    );
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
    scratch_dir: &'a Path,
    coverage_target_dir: &'a Path,
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
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt as _;

        Some(metadata.permissions().mode())
    };
    #[cfg(not(unix))]
    let mode = None;
    platform_permissions_allow(mode, requires_execute_bit(windows, cfg!(unix)))
}

fn platform_permissions_allow(mode: Option<u32>, require_execute_bit: bool) -> bool {
    !require_execute_bit || mode.is_some_and(has_executable_mode)
}

fn has_executable_mode(mode: u32) -> bool {
    mode & 0o111 != 0
}

fn requires_execute_bit(windows: bool, unix: bool) -> bool {
    unix && !windows
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

fn is_unsupported_arm64_windows_target(target: Option<&str>, toolchain: &ToolchainSelection) -> Result<bool, AppError> {
    if let Some(target) = target {
        return Ok(target == ARM64_WINDOWS_TARGET);
    }

    let mut rustc_version = Command::new(toolchain.rustc());
    rustc_version.arg("-vV");
    toolchain.apply_to_command(&mut rustc_version);
    let rustc_version = read_stdout(&mut rustc_version, "rustc host-target discovery")?;
    let host = rustc_host(&rustc_version).ok_or_else(|| AppError::new("`rustc -vV` did not report a host target"))?;
    Ok(host == ARM64_WINDOWS_TARGET)
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
    let evaluation_lcov = execution.scratch_dir.join(format!("lcov-{}.info", configuration.artifact_name()));

    run_clean(
        execution.workspace,
        execution.coverage_target_dir,
        execution.toolchain,
        execution.quiet,
    )?;
    run_nextest(execution, configuration)?;
    run_report(execution, configuration, &evaluation_lcov)?;
    publish_lcov(&evaluation_lcov, &final_lcov, &execution.args.coverage_dir, configuration)?;
    Ok(evaluation_lcov)
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

fn run_nextest(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration) -> Result<(), AppError> {
    let mut command = coverage_command(execution.workspace, execution.coverage_target_dir, execution.toolchain);
    command.args(["llvm-cov", "nextest", "--no-report"]);
    append_package_selection(&mut command, execution.selection);
    append_nextest_options(&mut command, execution.args, configuration, execution.target);
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

fn run_report(execution: &CollectionExecution<'_>, configuration: FeatureConfiguration, evaluation_lcov: &Path) -> Result<(), AppError> {
    #[cfg(not(windows))]
    let _ = configuration;
    let mut command = coverage_command(execution.workspace, execution.coverage_target_dir, execution.toolchain);
    command.args(["llvm-cov", "report", "--lcov", "--output-path"]).arg(evaluation_lcov);
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
    forward_output(&output, execution.quiet)?;
    if output.status.success() {
        return Ok(());
    }

    #[cfg(windows)]
    if let Some(arguments) = command_too_long_response_arguments(&String::from_utf8_lossy(&output.stderr)) {
        eprintln!("coverage-gate: cargo-llvm-cov could not launch llvm-cov directly; retrying its export through an LLVM response file");
        return run_windows_report_fallback(execution, configuration, evaluation_lcov, arguments);
    }

    Err(AppError::new(format!(
        "cargo llvm-cov report failed: `{display}` exited with {}",
        output.status
    )))
}

fn forward_output(output: &Output, quiet: bool) -> Result<(), AppError> {
    if !quiet {
        io::stdout()
            .write_all(&output.stdout)
            .into_app_err("failed to forward cargo-llvm-cov stdout")?;
    }
    io::stderr()
        .write_all(&output.stderr)
        .into_app_err("failed to forward cargo-llvm-cov stderr")
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
// Windows integration test covers the fallback command and published LCOV.
#[mutants::skip]
// The spawned-binary integration test covers this fallback, but the outer
// coverage report cannot include that child binary's coverage object.
#[cfg_attr(coverage_nightly, coverage(off))]
fn run_windows_report_fallback(
    execution: &CollectionExecution<'_>,
    configuration: FeatureConfiguration,
    evaluation_lcov: &Path,
    arguments: &str,
) -> Result<(), AppError> {
    let response = TemporaryPath::write_atomic(
        execution.scratch_dir,
        &format!("{}-objects.rsp", configuration.artifact_name()),
        arguments.as_bytes(),
    )?;
    remove_if_present(evaluation_lcov).into_app_err(format!(
        "failed to remove partial LCOV file `{}` before retry",
        evaluation_lcov.display()
    ))?;
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(evaluation_lcov)
        .into_app_err(format!("failed to create private LCOV file `{}`", evaluation_lcov.display()))?;
    let llvm_cov = discover_llvm_cov(execution.toolchain)?;
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
fn discover_llvm_cov(toolchain: &ToolchainSelection) -> Result<PathBuf, AppError> {
    if let Some(cov) = env::var_os("LLVM_COV") {
        return Ok(PathBuf::from(cov));
    }

    let mut command = Command::new(toolchain.rustc());
    command.args(["--print", "target-libdir"]);
    toolchain.apply_to_command(&mut command);
    let target_libdir = read_stdout(&mut command, "rustc LLVM-tool discovery")?;
    let rustlib = Path::new(target_libdir.trim())
        .parent()
        .ok_or_else(|| AppError::new("rustc target-libdir output had no parent directory"))?;
    Ok(rustlib.join("bin").join(format!("llvm-cov{}", env::consts::EXE_SUFFIX)))
}

fn publish_lcov(private_lcov: &Path, final_lcov: &Path, coverage_dir: &Path, configuration: FeatureConfiguration) -> Result<(), AppError> {
    publish_lcov_with(private_lcov, final_lcov, coverage_dir, configuration, io::copy)
}

fn publish_lcov_with(
    private_lcov: &Path,
    final_lcov: &Path,
    coverage_dir: &Path,
    configuration: FeatureConfiguration,
    copy: impl FnOnce(&mut File, &mut File) -> io::Result<u64>,
) -> Result<(), AppError> {
    let temporary_lcov = TemporaryPath::new(coverage_dir, &format!("{}.lcov", configuration.artifact_name()));
    let mut source = File::open(private_lcov).into_app_err(format!("failed to open private LCOV file `{}`", private_lcov.display()))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary_lcov.path())
        .into_app_err(format!(
            "failed to create temporary LCOV file `{}`",
            temporary_lcov.path().display()
        ))?;
    copy(&mut source, &mut output).into_app_err(format!(
        "failed to stage private LCOV file `{}` for publication",
        private_lcov.display()
    ))?;
    output
        .sync_all()
        .into_app_err(format!("failed to flush temporary LCOV file `{}`", temporary_lcov.path().display()))?;
    drop(output);

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
// Unix mutation jobs cannot execute this Windows FFI branch. Windows unit and
// integration tests cover replacement success, failure, and byte preservation.
#[mutants::skip]
fn replace_file_atomically(source: &Path, destination: &Path) -> io::Result<()> {
    use std::iter;
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let source = source.as_os_str().encode_wide().chain(iter::once(0)).collect::<Vec<_>>();
    let destination = destination.as_os_str().encode_wide().chain(iter::once(0)).collect::<Vec<_>>();
    retry_windows_replace(
        || {
            // SAFETY: both pointers reference live, NUL-terminated UTF-16
            // buffers for the duration of each call.
            let replaced = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), windows_replace_flags()) };
            if replaced != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
        },
        wait_before_windows_replace_retry,
    )
}

#[cfg(windows)]
// Sleeping is a trivial system delegation whose duration is intentionally not
// asserted; retry count and error selection are covered through injected waits.
#[mutants::skip]
// A real sharing violation is nondeterministic; retry scheduling is covered
// through the injected wait callback in retry_windows_replace.
#[cfg_attr(coverage_nightly, coverage(off))]
fn wait_before_windows_replace_retry() {
    std::thread::sleep(std::time::Duration::from_millis(10));
}

#[cfg(any(windows, test))]
fn retry_windows_replace(mut replace: impl FnMut() -> io::Result<()>, mut wait: impl FnMut()) -> io::Result<()> {
    for _ in 0..99 {
        match replace() {
            Ok(()) => return Ok(()),
            Err(error) if is_retryable_windows_replace_error(&error) => wait(),
            Err(error) => return Err(error),
        }
    }
    replace()
}

#[cfg(windows)]
// MOVEFILE_REPLACE_EXISTING and MOVEFILE_WRITE_THROUGH are disjoint bits, so
// cargo-mutants' `|` to `^` mutation is behaviorally equivalent.
#[mutants::skip]
fn windows_replace_flags() -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
}

#[cfg(any(windows, test))]
fn is_retryable_windows_replace_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32))
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
struct TemporaryPath {
    path: PathBuf,
    armed: bool,
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

impl TemporaryPath {
    fn new(directory: &Path, label: &str) -> Self {
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: directory.join(format!(".coverage-gate-{}-{sequence}-{label}", std::process::id())),
            armed: true,
        }
    }

    #[cfg(windows)]
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

    #[cfg(any(windows, test))]
    fn cleanup(mut self) -> Result<(), AppError> {
        remove_if_present(&self.path).into_app_err(format!("failed to remove temporary file `{}`", self.path.display()))?;
        self.armed = false;
        Ok(())
    }
}

#[cfg(any(windows, test))]
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

#[cfg(any(windows, test))]
fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
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
    }

    #[test]
    fn arm64_windows_detection_honors_explicit_target_without_invoking_rustc() {
        let toolchain = ToolchainSelection {
            name: None,
            cargo: OsString::from("unused-cargo"),
            rustc: OsString::from("unused-rustc"),
        };
        assert!(is_unsupported_arm64_windows_target(Some(ARM64_WINDOWS_TARGET), &toolchain).expect("explicit target needs no discovery"));
        assert!(
            !is_unsupported_arm64_windows_target(Some("x86_64-pc-windows-msvc"), &toolchain,).expect("explicit target needs no discovery")
        );
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
            Some(ARM64_WINDOWS_TARGET)
        );
        assert_eq!(
            cargo_llvm_cov_version("cargo-llvm-cov 0.9.0\n").expect("valid version"),
            Version::new(0, 9, 0)
        );
        let minimum = Version::new(0, 8, 0);
        assert!(!cargo_llvm_cov_is_supported(&Version::new(0, 7, 1), &minimum));
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

        assert_eq!(windows_replace_flags(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH);
    }

    #[test]
    fn windows_atomic_replace_retries_only_sharing_and_access_failures() {
        use std::cell::Cell;

        assert!(is_retryable_windows_replace_error(&io::Error::from_raw_os_error(5)));
        assert!(is_retryable_windows_replace_error(&io::Error::from_raw_os_error(32)));
        assert!(!is_retryable_windows_replace_error(&io::Error::from_raw_os_error(2)));

        let attempts = Cell::new(0);
        let waits = Cell::new(0);
        retry_windows_replace(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(io::Error::from_raw_os_error(32))
                } else {
                    Ok(())
                }
            },
            || waits.set(waits.get() + 1),
        )
        .expect("sharing violation is retried");
        assert_eq!(attempts.get(), 2);
        assert_eq!(waits.get(), 1);

        retry_windows_replace(
            || Err(io::Error::from_raw_os_error(2)),
            || panic!("non-retryable errors must not wait"),
        )
        .expect_err("non-retryable error must surface");

        let attempts = Cell::new(0);
        retry_windows_replace(
            || {
                attempts.set(attempts.get() + 1);
                Err(io::Error::from_raw_os_error(5))
            },
            || {},
        )
        .expect_err("the final retryable error must surface");
        assert_eq!(attempts.get(), 100);
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
    fn executable_mode_accepts_each_execute_bit() {
        assert!(has_executable_mode(0o100));
        assert!(has_executable_mode(0o010));
        assert!(has_executable_mode(0o001));
        assert!(!has_executable_mode(0));
        assert!(!has_executable_mode(0o600));
        assert!(platform_permissions_allow(None, false));
        assert!(platform_permissions_allow(Some(0o600), false));
        assert!(!platform_permissions_allow(None, true));
        assert!(!platform_permissions_allow(Some(0o600), true));
        assert!(platform_permissions_allow(Some(0o700), true));
        assert!(!requires_execute_bit(true, true));
        assert!(!requires_execute_bit(false, false));
        assert!(requires_execute_bit(false, true));
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
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn publishing_reports_an_unusable_temporary_directory() {
        let tmp = tempdir().expect("tempdir");
        let not_a_directory = tmp.path().join("not-a-directory");
        fs::write(&not_a_directory, b"file").expect("write conflicting file");
        let private = tmp.path().join("private.info");
        fs::write(&private, b"private").expect("write private LCOV");

        publish_lcov(
            &private,
            &tmp.path().join("unused.info"),
            &not_a_directory,
            FeatureConfiguration::AllFeatures,
        )
        .expect_err("temporary LCOV creation must fail");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn publishing_retains_invocation_private_evaluation_data() {
        let tmp = tempdir().expect("tempdir");
        let private = tmp.path().join("private.info");
        let published = tmp.path().join("published.info");
        fs::write(&private, b"private LCOV").expect("write private LCOV");

        publish_lcov(&private, &published, tmp.path(), FeatureConfiguration::AllFeatures).expect("publish LCOV");

        assert_eq!(fs::read(&private).expect("read private LCOV"), b"private LCOV");
        assert_eq!(fs::read(&published).expect("read published LCOV"), b"private LCOV");
    }

    #[test]
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn publishing_reports_copy_failures() {
        let tmp = tempdir().expect("tempdir");
        let private = tmp.path().join("private.info");
        let published = tmp.path().join("published.info");
        fs::write(&private, b"private LCOV").expect("write private LCOV");

        let error = publish_lcov_with(&private, &published, tmp.path(), FeatureConfiguration::AllFeatures, |_, _| {
            Err(io::Error::other("injected copy failure"))
        })
        .expect_err("copy failure must prevent publication");

        assert!(error.to_string().contains("failed to stage private LCOV file"));
        assert!(!published.exists());
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
    #[cfg_attr(miri, ignore = "uses temporary files, which Miri isolation does not support")]
    fn drop_removes_the_temporary_file() {
        let tmp = tempdir().expect("tempdir");
        let path = {
            let temporary = TemporaryPath::new(tmp.path(), "removed-on-drop");
            let path = temporary.path().to_path_buf();
            fs::write(&path, b"temporary").expect("write temporary file");
            path
        };

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

        let create_attempts = std::cell::Cell::new(0);
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
