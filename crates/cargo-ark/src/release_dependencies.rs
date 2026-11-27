// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};

use flate2::read::GzDecoder;
use serde_json::Value;
use tar::Archive;
use toml_edit::{DocumentMut, value};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// A validated set of release candidates.
#[derive(Debug)]
pub struct Candidates {
    packages: BTreeMap<String, String>,
}

impl Candidates {
    /// Parses and validates a candidate report.
    ///
    /// Identical entries count as one candidate. The report is rejected if one
    /// package name is associated with more than one version.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON is invalid, the required fields are not
    /// strings, or one package is reported with conflicting versions.
    pub fn from_report(report: &str) -> Result<Self> {
        let report: Vec<Value> = serde_json::from_str(report)?;
        let mut packages = BTreeMap::new();
        for candidate in report {
            let name = candidate["name"].as_str().ok_or("candidate is missing its name")?;
            let version = candidate["new_version"].as_str().ok_or("candidate is missing its version")?;
            if let Some(reported_version) = packages.insert(name.to_owned(), version.to_owned())
                && reported_version != version
            {
                return Err(format!("report lists two versions for `{name}`: `{reported_version}` and `{version}`; keep one").into());
            }
        }
        Ok(Self { packages })
    }

    /// Reports whether the candidate set contains no packages.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }
}

/// Validates release candidates by packaging and testing their archives.
///
/// The invoker owns the scratch directory for this call and is consumed after
/// the check completes. An empty candidate set succeeds without launching
/// Cargo, although callers can avoid preparing an invoker for that case.
///
/// # Errors
///
/// Returns an error if scratch cannot be used, packaging fails, an archive is
/// missing or cannot be extracted, or a shipped test fails.
#[expect(
    clippy::needless_pass_by_value,
    reason = "Taking ownership keeps the invoker and its scratch directory single-use."
)]
pub fn check(candidates: Candidates, cargo: CargoInvoker) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }

    let archives = package_candidates(&cargo, &candidates.packages)?;
    validate_packages(&cargo, &archives, candidates.packages)
}

fn package_candidates(cargo: &CargoInvoker, candidates: &BTreeMap<String, String>) -> Result<PathBuf> {
    // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, reason = "Both Cargo and archive lookup use this private directory under fresh scratch; neither replacement collides with other scratch paths.")]
    let artifacts = cargo.scratch.path().join("artifacts");
    let mut package = cargo.package_command(candidates);
    run(&mut package, "cargo package for release candidates", Command::status)?;
    Ok(artifacts.join("package"))
}

fn validate_packages(cargo: &CargoInvoker, archives: &Path, candidates: BTreeMap<String, String>) -> Result<()> {
    let sources = unpack_candidates(archives, cargo.scratch.path(), candidates)?;
    test_archives(cargo, &sources)
}

fn unpack_candidates(archives: &Path, scratch: &Path, candidates: BTreeMap<String, String>) -> Result<BTreeMap<String, PathBuf>> {
    let mut sources = BTreeMap::new();
    for (index, (name, version)) in candidates.into_iter().enumerate() {
        let stem = format!("{name}-{version}");
        let archive = archives.join(format!("{stem}.crate"));
        // Separate parents keep a sibling's files out of the original relative layout.
        // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, reason = "The numbered parents remain separate, under fresh scratch, and the same paths are used for extraction and testing.")]
        let destination = scratch.join("sources").join(index.to_string());
        File::open(&archive)
            .and_then(|file| Archive::new(GzDecoder::new(file)).unpack(&destination))
            .map_err(|error| format!("extracting {}: {error}", archive.display()))?;
        sources.insert(name, destination.join(stem));
    }
    Ok(sources)
}

fn test_archives(cargo: &CargoInvoker, sources: &BTreeMap<String, PathBuf>) -> Result<()> {
    // #[gamma::skip(literal.str_to_xyzzy, reason = "The same private file is written and passed to Cargo by explicit --config path.")]
    let patch = cargo.scratch.path().join("candidates.toml");
    fs::write(&patch, patch_config(sources)?.to_string())?;
    for (name, source) in sources {
        let manifest = source.join("Cargo.toml");
        for feature_flag in ARCHIVE_FEATURE_MODES {
            let mode = feature_flag.unwrap_or("(default features)");
            run(
                &mut archive_test_command(cargo, &patch, &manifest, feature_flag),
                &format!("{name}: cargo test {mode} from package archive"),
                Command::status,
            )?;
        }
    }
    Ok(())
}

const ARCHIVE_FEATURE_MODES: [Option<&str>; 3] = [Some("--all-features"), Some("--no-default-features"), None];

fn archive_test_command(cargo: &CargoInvoker, patch: &Path, manifest: &Path, feature_flag: Option<&str>) -> Command {
    let mut command = cargo.archive_command(patch);
    command.arg("test").args(feature_flag).arg("--manifest-path").arg(manifest);
    command
}

fn patch_config(sources: &BTreeMap<String, PathBuf>) -> Result<DocumentMut> {
    let mut config = DocumentMut::new();
    for (name, source) in sources {
        let path = source.to_str().ok_or("candidate archive path must be UTF-8")?;
        // Patch the dependency source, not a publish destination or mirror.
        config["patch"]["crates-io"][name.as_str()]["path"] = value(path);
    }
    Ok(config)
}

/// Configures Cargo commands for one release-dependency check.
///
/// The invoker owns the check's scratch directory. It can be passed to
/// [`check`] once; moving it into that call prevents a scratch directory from
/// being reused by another check.
#[derive(Debug)]
pub struct CargoInvoker {
    root: PathBuf,
    child_env: BTreeMap<OsString, OsString>,
    scratch: tempfile::TempDir,
}

impl CargoInvoker {
    /// Prepares Cargo for a known workspace without launching any processes.
    ///
    /// Cargo runs from `workspace_root` with the supplied child environment
    /// overrides. Callers must select the desired toolchain and supply an
    /// absolute path if setting `CARGO_HOME`. Use [`Self::discover`] to find
    /// the workspace and toolchain from an invocation directory instead.
    /// `temp_dir`, when present, selects the existing parent of the private
    /// scratch directory.
    ///
    /// # Errors
    ///
    /// Returns an error if either path cannot be resolved, scratch cannot be
    /// created, or its parent is the workspace or a directory inside it.
    pub fn new(workspace_root: &Path, child_env: BTreeMap<OsString, OsString>, temp_dir: Option<&Path>) -> Result<Self> {
        let root = workspace_root.canonicalize()?;
        let temporary = temp_dir.map_or_else(std::env::temp_dir, Path::to_path_buf);
        let scratch_error = |error| format!("preparing release scratch directory in {}: {error}", temporary.display());
        let parent = temporary.canonicalize().map_err(scratch_error)?;
        if parent.starts_with(&root) {
            return Err("select a temporary directory outside the source workspace".into());
        }
        let scratch = tempfile::tempdir_in(parent).map_err(scratch_error)?;
        Ok(Self { root, child_env, scratch })
    }

    /// Discovers a workspace and prepares one Cargo invocation context.
    ///
    /// Discovery starts in `invocation_dir`. A `CARGO_HOME` override in
    /// `child_env` takes precedence over the inherited environment. A nonempty
    /// value is made absolute relative to `invocation_dir` before running
    /// Cargo. `temp_dir`, when present, selects the existing parent directory
    /// for the private scratch area.
    ///
    /// # Errors
    ///
    /// Returns an error if the invocation directory, `CARGO_HOME`, or scratch
    /// parent cannot be resolved, Cargo or rustc cannot be executed, discovery
    /// reports a failure, or the scratch parent resolves to the workspace
    /// itself or a child directory.
    pub fn discover(invocation_dir: PathBuf, child_env: BTreeMap<OsString, OsString>, temp_dir: Option<&Path>) -> Result<Self> {
        Self::discover_with(invocation_dir, child_env, std::env::var_os("CARGO_HOME"), temp_dir, Command::output)
    }

    fn discover_with(
        invocation_dir: PathBuf,
        mut child_env: BTreeMap<OsString, OsString>,
        inherited_cargo_home: Option<OsString>,
        temp_dir: Option<&Path>,
        mut run_output: impl FnMut(&mut Command) -> io::Result<Output>,
    ) -> Result<Self> {
        let invocation_dir = std::path::absolute(invocation_dir)?;
        if let Some(home) = child_env.remove(OsStr::new("CARGO_HOME")).or(inherited_cargo_home) {
            let home = if home.is_empty() {
                home
            } else {
                std::path::absolute(invocation_dir.join(home))?.into()
            };
            child_env.insert("CARGO_HOME".into(), home);
        }

        let mut locate = Command::new("cargo");
        locate
            .current_dir(&invocation_dir)
            .args(["locate-project", "--workspace", "--message-format", "plain"])
            .envs(&child_env);
        let manifest = capture(&mut locate, "locating the Cargo workspace", &mut run_output)?;
        let manifest = manifest.trim_end_matches(['\r', '\n']);
        let root = Path::new(manifest).parent().ok_or("workspace manifest has no parent directory")?;

        let mut select_toolchain = Command::new("rustc");
        select_toolchain
            .current_dir(&invocation_dir)
            .args(["--print", "sysroot"])
            .envs(&child_env);
        let toolchain = capture(&mut select_toolchain, "selecting Rust toolchain", run_output)?;
        child_env.insert("RUSTUP_TOOLCHAIN".into(), toolchain.trim().into());
        Self::new(root, child_env, temp_dir)
    }

    fn command(&self) -> Command {
        let mut command = Command::new("cargo");
        // #[gamma::skip(literal.str_to_xyzzy, reason = "Renaming the private build subdirectory leaves Cargo's output isolated in scratch.")]
        let build = self.scratch.path().join("build");
        command
            // Cargo loads configuration from this directory, not from --manifest-path.
            .current_dir(&self.root)
            .envs(&self.child_env)
            .env("CARGO_BUILD_BUILD_DIR", build);
        command
    }

    fn package_command(&self, candidates: &BTreeMap<String, String>) -> Command {
        let artifacts = self.scratch.path().join("artifacts");
        let mut command = self.command();
        command.args(["package", "--allow-dirty", "--target-dir"]).arg(&artifacts);
        for (name, version) in candidates {
            command.arg("--package").arg(format!("{name}@{version}"));
        }
        command
    }

    fn archive_command(&self, patch: &Path) -> Command {
        let mut command = self.command();
        // #[gamma::skip(literal.str_to_xyzzy, reason = "Renaming the private target subdirectory leaves Cargo's output isolated in scratch.")]
        let target = self.scratch.path().join("target");
        command.env("CARGO_TARGET_DIR", target).arg("--config").arg(patch);
        command
    }
}

fn capture(command: &mut Command, operation: &str, run_output: impl FnOnce(&mut Command) -> io::Result<Output>) -> Result<String> {
    let output = run_output(command.stderr(Stdio::inherit()))?;
    if !output.status.success() {
        return Err(format!("{operation} failed ({})", output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn run(command: &mut Command, operation: &str, run_status: impl FnOnce(&mut Command) -> io::Result<ExitStatus>) -> Result<()> {
    let status = run_status(command)?;
    if !status.success() {
        return Err(format!("{operation} failed ({status})").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
