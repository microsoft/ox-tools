// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provenance for the tool-owned, dependency-free guard runtime, which lives outside the workspace.

#[cfg(not(test))]
use std::env as capture_env;

#[cfg(test)]
use super::snapshot_io::environment as capture_env;
use super::*;

const FILE: &str = "runtime-provenance";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Runtime {
    pub(super) root: Utf8PathBuf,
    pub(super) files: Vec<(Utf8PathBuf, [u8; 32])>,
    pub(super) shape: [u8; 32],
    pub(super) artifacts: Vec<(Utf8PathBuf, [u8; 32])>,
    compiler: (Utf8PathBuf, [u8; 32]),
    pub(super) context: [u8; 32],
}

impl Runtime {
    pub(super) fn valid(&self, files: &impl Files, root: &Utf8Path, compiler: Option<&Utf8Path>) -> bool {
        self.root == root
            && compiler.is_none_or(|compiler| self.compiler.0 == compiler)
            && digest(files, &self.compiler.0) == Some(self.compiler.1)
            // #[gamma::skip(literal.str_to_xyzzy, tag = "equivalent", reason = "Native runtime paths are absolute and cannot start with the relative normal component xyzzy.")]
            && inventory(files, root, Utf8Path::new("")) == Some(self.shape)
            && !self.files.is_empty()
            && self.files.iter().all(|(path, hash)| digest(files, path) == Some(*hash))
    }

    pub(super) fn contains(&self, files: &impl Files, artifact: &Utf8Path) -> bool {
        self.artifacts
            .iter()
            .any(|(path, hash)| path == artifact && digest(files, path) == Some(*hash))
    }
}

pub(super) fn load(files: &impl Files, captures: &Utf8Path) -> Option<Runtime> {
    serde_json::from_slice(&files.read(&captures.join(FILE))?).ok()
}

/// This is artifact provenance, not compiler-result reuse: only the tool's vendored, dependency-free
/// runtime is accepted. Its artifact bytes are always inputs to the consuming unit.
/// Returns `Some(())` only after publication; unavailable context, unsupported input or a
/// publication failure returns `None`. Callers may decline this optional optimization.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn capture(command: &Command, environment: &BTreeMap<String, String>) -> Option<()> {
    let cwd = capture_env::current_dir()
        .ok()
        .and_then(|path| Utf8PathBuf::from_path_buf(path).ok())?;
    let capture = capture_with(&NativeFiles, command, environment, &cwd)?;
    let directory = environment.get(CAPTURE_DIR_VAR)?;
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Runtime derives Serialize over UTF-8 paths, strings, arrays and integers; serialization into a Vec cannot fail.")]
    let bytes = serde_json::to_vec(&capture).ok()?;
    publish_atomic(&Utf8Path::new(directory).join(FILE), &bytes).ok()
}

pub(super) fn capture_with(
    files: &impl Files,
    command: &Command,
    environment: &BTreeMap<String, String>,
    cwd: &Utf8Path,
) -> Option<Runtime> {
    let files = &SnapshotFiles::new(files);
    let root = Utf8PathBuf::from(environment.get(RUNTIME_VAR)?);
    let target = Utf8PathBuf::from(environment.get(TARGET_VAR)?);
    let manifest = Utf8PathBuf::from(environment.get("CARGO_MANIFEST_DIR")?);
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The native wrapper constructs this compiler command from a UTF-8 compiler path before executing it.")]
    let compiler = Utf8PathBuf::from(command.get_program().to_str()?);
    let actual_args: Vec<_> = command.get_args().map(OsString::from).collect();
    let invocation = parse_invocation(&actual_args)?;
    if (
        // #[gamma::skip(logical.or_remove_left, tag = "equivalent", reason = "A relative root must equal manifest but cannot equal the later absolute source path, so source equality independently declines it.")]
        !root.is_absolute() || manifest != root
    )
        || !compiler.is_absolute()
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The native caller records runtime provenance only after this compiler executable succeeded; a filesystem root cannot execute.")]
        || compiler.file_stem()? != "rustc"
        || invocation.crate_name != "gamma_rt"
        || invocation.test
        || invocation.opaque_extern
        || !invocation.externs.is_empty()
        || !invocation.crate_types.iter().all(|kind| matches!(kind.as_str(), "lib" | "rlib"))
        || absolute(&invocation.source, cwd) != root.join("src").join("lib.rs")
        ||
        // #[gamma::skip(collection.reverse_array, tag = "equivalent", reason = "The immutable environment-key array is used only by pure any(contains_key).")]
        ["OUT_DIR", ORIGINAL_WRAPPER_VAR, "RUSTC_WORKSPACE_WRAPPER"]
            .iter()
            .any(|name| environment.contains_key(*name))
        || !environment.contains_key(COMMAND_VAR)
    {
        return None;
    }
    let mut args = actual_args.clone();
    for arg in &mut args {
        if arg == "--emit=dep-info,link" || arg == "--emit=dep-info,metadata,link" {
            *arg = "--emit=dep-info,metadata".into();
        }
    }
    let _searches = metadata_arguments(&args)?;
    let manifest = toml::from_str::<toml::Value>(&String::from_utf8(files.read(&root.join("Cargo.toml"))?).ok()?).ok()?;
    if manifest.get("package")?.get("name")?.as_str()? != "cargo-gamma-rt" || !dependency_free(&manifest) {
        return None;
    }
    // #[gamma::skip(literal.str_to_xyzzy, tag = "equivalent", reason = "Native runtime paths are absolute and cannot start with the relative normal component xyzzy.")]
    let listing = files.files(&root, Utf8Path::new(""))?;
    let mut paths = Vec::with_capacity(listing.len());
    for path in listing {
        paths.push((path.clone(), digest(files, &path)?));
    }
    let out = absolute(&invocation.out_dir, cwd);
    if !out.starts_with(&target) {
        return None;
    }
    let dep = out.join(format!("{}{}.d", invocation.crate_name, invocation.extra_filename));
    let text = String::from_utf8(files.read(&dep)?).ok()?;
    let mut artifacts = Vec::new();
    for line in text.lines() {
        let Some((output, _sources)) = line.split_once(": ") else {
            continue;
        };
        let output = super::super::build::messages::dependencies(output);
        if output.len() != 1 {
            return None;
        }
        let output = absolute(Utf8Path::new(&output[0]), cwd);
        if matches!(output.extension(), Some("rlib" | "rmeta")) {
            // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The immediately preceding rlib/rmeta extension match guarantees an output filename and stem.")]
            if !output.starts_with(&target) || output.file_stem()? != format!("libgamma_rt{}", invocation.extra_filename) {
                return None;
            }
            artifacts.push((output.clone(), digest(files, &output)?));
        }
    }
    // #[gamma::skip(logical.or_remove_right, tag = "equivalent", reason = "The native complete root inventory includes the Cargo.toml successfully read above; an absent or unreadable inventory already declined.")]
    if artifacts.is_empty() || paths.is_empty() {
        return None;
    }
    let mut identity = Command::new(&compiler);
    let _identity = identity.current_dir(cwd).args(&actual_args);
    Some(Runtime {
        root: root.clone(),
        files: paths,
        shape:
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The earlier runtime listing memoized this same root and exclusion key successfully.")]
        inventory(files, &root, Utf8Path::new(""))?, // #[gamma::skip(literal.str_to_xyzzy, tag = "equivalent", reason = "Native runtime paths are absolute and cannot start with the relative normal component xyzzy.")]
        artifacts,
        compiler: (compiler.clone(), digest(files, &compiler)?),
        context: command_fingerprint(&identity, environment.iter().map(|(name, value)| (name.into(), value.into())))?, // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "metadata_arguments validated every actual argument as UTF-8; program, working directory, and environment are already UTF-8.")]
    })
}

fn dependency_free(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table.iter().all(|(key, value)| {
            if key.ends_with("dependencies") {
                value.as_table().is_some_and(toml::map::Map::is_empty)
            } else if key == "build" || key == "proc-macro" {
                value.as_bool() == Some(false)
            } else {
                dependency_free(value)
            }
        }),
        toml::Value::Array(values) => values.iter().all(dependency_free),
        _ => true,
    }
}
