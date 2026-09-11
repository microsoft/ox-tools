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
use serde_json::Value;

use crate::cli::{CollectionArgs, CoverageGateArgs, FeatureConfiguration};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn run(args: &CoverageGateArgs, collection: &CollectionArgs) -> Result<ExitCode, AppError> {
    if !args.lcov.is_empty() {
        return Err(AppError::new(
            "`--lcov` cannot be used with `cargo coverage-gate run`; collected LCOV paths are selected by `--coverage-dir`",
        ));
    }
    let workspace = WorkspaceInfo::load()?;
    let selection = Selection::resolve(&workspace, &args.packages, collection.package_file.as_deref())?;
    if selection.explicit && selection.members.is_empty() {
        eprintln!("coverage-gate: package selection is empty; nothing to do");
        return Ok(ExitCode::SUCCESS);
    }

    let mut collection = collection.clone();
    collection.coverage_dir = absolute_path(&collection.coverage_dir)?;
    let configurations = normalized_configurations(&collection.configurations);
    fs::create_dir_all(&collection.coverage_dir).into_app_err(format!(
        "failed to create coverage directory `{}`",
        collection.coverage_dir.display()
    ))?;

    let tools = LlvmTools::discover()?;
    let coverage_target_dir = workspace.target_dir.join("llvm-cov-target");
    let mut lcov_paths = Vec::with_capacity(configurations.len());
    for configuration in configurations {
        let lcov_path = collect_configuration(
            &workspace,
            &selection,
            &collection,
            configuration,
            &coverage_target_dir,
            &tools,
            args.target.as_deref(),
        )?;
        lcov_paths.push(lcov_path);
    }

    crate::run::evaluate_paths(args, &lcov_paths, &selection.gated_names())
}

fn absolute_path(path: &Path) -> Result<PathBuf, AppError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()
            .into_app_err("failed to resolve the current directory")?
            .join(path))
    }
}

#[derive(Debug)]
struct WorkspaceInfo {
    root: PathBuf,
    target_dir: PathBuf,
    members: Vec<WorkspaceMember>,
}

impl WorkspaceInfo {
    fn load() -> Result<Self, AppError> {
        let mut command = MetadataCommand::new();
        command.no_deps();
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
    glob_matches_from(&pattern, 0, &name, 0)
}

fn glob_matches_from(pattern: &[char], mut pattern_index: usize, name: &[char], mut name_index: usize) -> bool {
    while pattern_index < pattern.len() {
        match pattern[pattern_index] {
            '?' => {
                if name_index == name.len() {
                    return false;
                }
                pattern_index += 1;
                name_index += 1;
            }
            '*' => {
                while pattern.get(pattern_index + 1) == Some(&'*') {
                    pattern_index += 1;
                }
                pattern_index += 1;
                if pattern_index == pattern.len() {
                    return true;
                }
                return (name_index..=name.len()).any(|candidate| glob_matches_from(pattern, pattern_index, name, candidate));
            }
            expected => {
                if name.get(name_index) != Some(&expected) {
                    return false;
                }
                pattern_index += 1;
                name_index += 1;
            }
        }
    }
    name_index == name.len()
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

fn collect_configuration(
    workspace: &WorkspaceInfo,
    selection: &Selection,
    args: &CollectionArgs,
    configuration: FeatureConfiguration,
    coverage_target_dir: &Path,
    tools: &LlvmTools,
    target: Option<&str>,
) -> Result<PathBuf, AppError> {
    let final_lcov = args.coverage_dir.join(format!("lcov-{}.info", configuration.artifact_name()));

    run_clean(workspace, coverage_target_dir)?;
    let objects = run_nextest(workspace, selection, args, configuration, coverage_target_dir, target)?;
    if objects.is_empty() {
        return Err(AppError::new(format!(
            "cargo-llvm-cov produced no executable object paths for the `{}` configuration",
            configuration.artifact_name()
        )));
    }

    let profraw_files = discover_profraw_files(coverage_target_dir)?;
    if profraw_files.is_empty() {
        return Err(AppError::new(format!(
            "cargo-llvm-cov produced no raw profiles for the `{}` configuration",
            configuration.artifact_name()
        )));
    }

    let profile_list = TemporaryPath::write_atomic(
        &args.coverage_dir,
        &format!("{}-profraw-list", configuration.artifact_name()),
        &profile_list_contents(&profraw_files)?,
    )?;
    let profdata = TemporaryPath::new(&args.coverage_dir, &format!("{}.profdata", configuration.artifact_name()));
    run_profdata_merge(tools, profile_list.path(), profdata.path())?;

    let response = TemporaryPath::write_atomic(
        &args.coverage_dir,
        &format!("{}-objects.rsp", configuration.artifact_name()),
        &object_response_contents(&objects)?,
    )?;
    export_lcov(
        workspace,
        tools,
        profdata.path(),
        response.path(),
        &final_lcov,
        &args.coverage_dir,
        configuration,
    )?;

    response.cleanup()?;
    profile_list.cleanup()?;
    profdata.cleanup()?;
    Ok(final_lcov)
}

fn cargo_program() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

fn coverage_command(workspace: &WorkspaceInfo, coverage_target_dir: &Path) -> Command {
    let mut command = Command::new(cargo_program());
    command
        .current_dir(&workspace.root)
        .env("CARGO_LLVM_COV_TARGET_DIR", coverage_target_dir)
        .env("CARGO_LLVM_COV_BUILD_DIR", coverage_target_dir);
    command
}

fn run_clean(workspace: &WorkspaceInfo, coverage_target_dir: &Path) -> Result<(), AppError> {
    let mut command = coverage_command(workspace, coverage_target_dir);
    command.args(["llvm-cov", "clean", "--workspace"]);
    run_status(&mut command, "cargo llvm-cov clean")
}

fn run_nextest(
    workspace: &WorkspaceInfo,
    selection: &Selection,
    args: &CollectionArgs,
    configuration: FeatureConfiguration,
    coverage_target_dir: &Path,
    target: Option<&str>,
) -> Result<Vec<PathBuf>, AppError> {
    let mut command = coverage_command(workspace, coverage_target_dir);
    command.args(["llvm-cov", "nextest", "--no-report"]);
    if selection.explicit {
        for member in &selection.members {
            command.arg("--package").arg(member.spec());
        }
    } else {
        command.arg("--workspace");
    }
    command.arg(configuration.cargo_flag());
    if let Some(target) = target {
        command.arg("--target").arg(target);
    }
    if let Some(jobs) = args.jobs {
        command.arg("--jobs").arg(jobs.get().to_string());
        command.arg("--build-jobs").arg(jobs.get().to_string());
    }
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
            Err(_error) => println!("{line}"),
        }
    }
    let status = child.wait().into_app_err(format!("failed to wait for `{display}`"))?;
    if !status.success() {
        return Err(AppError::new(format!("`{display}` exited with {status}")));
    }
    Ok(objects.into_iter().collect())
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
    let extension = path.extension().unwrap_or_default();
    if extension == "d"
        || extension == "rlib"
        || extension == "rmeta"
        || path.ends_with(".cargo-lock")
        || path.ends_with(".cargo-build-lock")
    {
        return false;
    }
    #[cfg(windows)]
    {
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

        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
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

fn run_profdata_merge(tools: &LlvmTools, profile_list: &Path, output: &Path) -> Result<(), AppError> {
    let mut command = Command::new(&tools.profdata);
    command.args(["merge", "-sparse", "-f"]).arg(profile_list).arg("-o").arg(output);
    append_space_separated_env(&mut command, "LLVM_PROFDATA_FLAGS");
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

    fs::rename(temporary_lcov.path(), final_lcov).into_app_err(format!("failed to publish LCOV file `{}`", final_lcov.display()))?;
    temporary_lcov.disarm();
    Ok(())
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
    fn discover() -> Result<Self, AppError> {
        let cov = env::var_os("LLVM_COV").map(PathBuf::from);
        let profdata = env::var_os("LLVM_PROFDATA").map(PathBuf::from);
        if let (Some(cov), Some(profdata)) = (cov.clone(), profdata.clone()) {
            return Ok(Self { cov, profdata });
        }

        let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let output = Command::new(&rustc)
            .args(["--print", "target-libdir"])
            .output()
            .into_app_err(format!("failed to execute `{}` to locate LLVM tools", Path::new(&rustc).display()))?;
        if !output.status.success() {
            return Err(AppError::new(format!(
                "`{} --print target-libdir` exited with {}: {}",
                Path::new(&rustc).display(),
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
    fn profile_and_object_lists_reject_non_utf8_paths() {
        let path = invalid_unicode_path();
        profile_list_contents(std::slice::from_ref(&path)).expect_err("profile paths must be UTF-8");
        object_response_contents(&[path]).expect_err("object paths must be UTF-8");
    }

    #[test]
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
    fn remove_if_present_reports_a_directory() {
        let tmp = tempdir().expect("tempdir");
        let directory = tmp.path().join("directory");
        fs::create_dir(&directory).expect("create directory");

        remove_if_present(&directory).expect_err("remove_file must reject a directory");
    }

    #[test]
    fn atomic_rename_reports_a_missing_staging_file() {
        let tmp = tempdir().expect("tempdir");
        let staging = TemporaryPath::new(tmp.path(), "missing");
        let published = TemporaryPath::new(tmp.path(), "published");

        atomic_rename(&staging, &published).expect_err("a missing staging file must fail publication");
    }

    #[test]
    fn absolute_paths_are_preserved() {
        let absolute = env::current_dir().expect("current directory").join("coverage");
        assert_eq!(absolute_path(&absolute).expect("absolute path"), absolute);
    }

    #[test]
    fn relative_paths_are_anchored_to_the_invocation_directory() {
        let current = env::current_dir().expect("current directory");
        assert_eq!(
            absolute_path(Path::new("coverage")).expect("relative path"),
            current.join("coverage")
        );
    }
}
