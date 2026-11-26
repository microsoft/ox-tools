// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use flate2::read::GzDecoder;
use serde_json::Value;
use tar::Archive;
use toml_edit::{DocumentMut, value};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// # Errors
///
/// Returns an error if the candidate list cannot be read, scratch cannot be
/// created outside the workspace, or packaging or archive testing fails.
pub(super) fn check(temp_dir: Option<&Path>) -> Result<()> {
    let mut json = String::new();
    io::stdin().read_to_string(&mut json)?;
    let candidates = parse_candidates(&json)?;
    if candidates.is_empty() {
        println!("cargo ark release-dependencies: no release candidates");
        return Ok(());
    }

    let cargo = ReleaseCargo::new()?;
    let scratch = scratch_directory(&cargo.root, temp_dir)?;
    let archives = package_candidates(&cargo, scratch.path(), &candidates)?;
    let sources = unpack_candidates(&archives, scratch.path(), candidates)?;
    test_archives(&cargo, scratch.path(), &sources)
}

fn parse_candidates(json: &str) -> Result<BTreeMap<String, String>> {
    let report: Vec<Value> = serde_json::from_str(json)?;
    report
        .iter()
        .map(|candidate| {
            let name = candidate["name"].as_str().ok_or("candidate is missing its name")?;
            let version = candidate["new_version"].as_str().ok_or("candidate is missing its version")?;
            Ok((name.to_owned(), version.to_owned()))
        })
        .collect()
}

fn scratch_directory(root: &Path, temp_dir: Option<&Path>) -> Result<tempfile::TempDir> {
    let temporary = std::path::absolute(temp_dir.map_or_else(std::env::temp_dir, Path::to_path_buf))?;
    let scratch = tempfile::Builder::new()
        .tempdir_in(&temporary)
        .map_err(|error| format!("creating release scratch directory in {}: {error}", temporary.display()))?;
    if scratch.path().canonicalize()?.starts_with(root.canonicalize()?) {
        return Err("select a temporary directory outside the source workspace".into());
    }
    Ok(scratch)
}

fn package_candidates(cargo: &ReleaseCargo, scratch: &Path, candidates: &BTreeMap<String, String>) -> Result<PathBuf> {
    // #[gamma::skip(literal.str_to_empty, literal.str_to_xyzzy, reason = "Both Cargo and archive lookup use this private directory under fresh scratch; neither replacement collides with other scratch paths.")]
    let artifacts = scratch.join("artifacts");
    let mut package = cargo.command(scratch);
    package.args(["package", "--allow-dirty", "--target-dir"]).arg(&artifacts);
    for (name, version) in candidates {
        package.arg("--package").arg(format!("{name}@{version}"));
    }
    run(&mut package, "cargo package for release candidates")?;
    Ok(artifacts.join("package"))
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

fn test_archives(cargo: &ReleaseCargo, scratch: &Path, sources: &BTreeMap<String, PathBuf>) -> Result<()> {
    // #[gamma::skip(literal.str_to_xyzzy, reason = "The same private file is written and passed to Cargo by explicit --config path.")]
    let patch = scratch.join("candidates.toml");
    fs::write(&patch, patch_config(sources)?.to_string())?;
    for (name, source) in sources {
        let manifest = source.join("Cargo.toml");
        for features in ["--all-features", "--no-default-features"] {
            run(
                cargo
                    .archive_command(scratch, &patch)
                    .args(["test", features, "--manifest-path"])
                    .arg(&manifest),
                &format!("{name}: cargo test {features} from package archive"),
            )?;
        }
    }
    Ok(())
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

struct ReleaseCargo {
    root: PathBuf,
    toolchain: String,
}

impl ReleaseCargo {
    fn new() -> Result<Self> {
        let manifest = capture(
            Command::new("cargo").args(["locate-project", "--workspace", "--message-format", "plain"]),
            "locating the Cargo workspace",
        )?;
        let root = Path::new(&manifest)
            .parent()
            .ok_or("workspace manifest has no parent directory")?
            .to_path_buf();
        let toolchain = capture(Command::new("rustc").args(["--print", "sysroot"]), "selecting Rust toolchain")?;
        Ok(Self { root, toolchain })
    }

    fn command(&self, scratch: &Path) -> Command {
        let mut command = Command::new("cargo");
        // #[gamma::skip(literal.str_to_xyzzy, reason = "Renaming the private build subdirectory leaves Cargo's output isolated in scratch.")]
        let build = scratch.join("build");
        command
            // Cargo loads configuration from this directory, not from --manifest-path.
            .current_dir(&self.root)
            .env("RUSTUP_TOOLCHAIN", self.toolchain.trim())
            .env("CARGO_BUILD_BUILD_DIR", build);
        command
    }

    fn archive_command(&self, scratch: &Path, patch: &Path) -> Command {
        let mut command = self.command(scratch);
        // #[gamma::skip(literal.str_to_xyzzy, reason = "Renaming the private target subdirectory leaves Cargo's output isolated in scratch.")]
        let target = scratch.join("target");
        command.env("CARGO_TARGET_DIR", target).arg("--config").arg(patch);
        command
    }
}

fn capture(command: &mut Command, operation: &str) -> Result<String> {
    let output = command.stderr(Stdio::inherit()).output()?;
    if !output.status.success() {
        return Err(format!("{operation} failed ({})", output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn run(command: &mut Command, operation: &str) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{operation} failed ({status})").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn reads_candidate_versions_and_ignores_discovery_metadata() -> Result<()> {
        let report = r#"[{"name":"peer","new_version":"1.2.0","old_version":"1.1.0","classification":"compatible"}]"#;
        assert_eq!(parse_candidates(report)?, BTreeMap::from([("peer".into(), "1.2.0".into())]));
        assert!(parse_candidates("[]")?.is_empty());
        Ok(())
    }

    #[test]
    fn rejects_invalid_candidate_reports() {
        for report in ["", "{}"] {
            assert!(parse_candidates(report).is_err(), "{report}");
        }
        for (report, diagnostic) in [
            (r#"[{"new_version":"1.2.0"}]"#, "candidate is missing its name"),
            (r#"[{"name":"peer","old_version":"1.1.0"}]"#, "candidate is missing its version"),
        ] {
            assert_eq!(parse_candidates(report).unwrap_err().to_string(), diagnostic);
        }
    }

    #[test]
    fn reports_the_candidate_archive_that_could_not_be_extracted() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let archives = scratch.path().join("package");
        let candidates = BTreeMap::from([("missing".into(), "1.2.0".into())]);
        let error = unpack_candidates(&archives, scratch.path(), candidates).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("extracting {}", archives.join("missing-1.2.0.crate").display()))
        );
        Ok(())
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn reports_paths_cargo_cannot_represent_in_patch_toml() {
        #[cfg(unix)]
        let directory = {
            use std::os::unix::ffi::OsStringExt;
            std::ffi::OsString::from_vec(vec![0xff])
        };
        #[cfg(windows)]
        let directory = {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&[0xd800])
        };
        let sources = BTreeMap::from([("candidate".into(), directory.into())]);
        assert_eq!(
            patch_config(&sources).unwrap_err().to_string(),
            "candidate archive path must be UTF-8"
        );
    }

    #[test]
    fn reports_when_the_required_cargo_process_cannot_start() -> Result<()> {
        let scratch = tempfile::tempdir()?;
        let mut missing = Command::new(scratch.path().join("unavailable-cargo"));
        for error in [
            capture(&mut missing, "locating the Cargo workspace").unwrap_err(),
            run(&mut missing, "testing a candidate").unwrap_err(),
        ] {
            assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), io::ErrorKind::NotFound);
        }
        Ok(())
    }

    #[test]
    fn archive_commands_use_workspace_config_and_keep_builds_in_scratch() {
        let root = PathBuf::from("workspace");
        let scratch = PathBuf::from("scratch");
        let patch = scratch.join("patch.toml");
        let cargo = ReleaseCargo {
            root: root.clone(),
            toolchain: "workspace-toolchain\n".into(),
        };
        let command = cargo.archive_command(&scratch, &patch);
        assert_eq!(command.get_current_dir(), Some(root.as_path()));
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--config".as_ref(), patch.as_os_str()]);
        let env: BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env.get(OsStr::new("RUSTUP_TOOLCHAIN")),
            Some(&Some(OsStr::new("workspace-toolchain")))
        );
        for key in ["CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"] {
            let directory = env.get(OsStr::new(key)).copied().flatten().map(Path::new).unwrap();
            assert!(directory.starts_with(&scratch) && directory != scratch);
        }
    }
}
