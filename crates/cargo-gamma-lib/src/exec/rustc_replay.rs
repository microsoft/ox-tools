// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conservative compiler-result reuse underneath Cargo's scheduler.
//!
//! Cargo still constructs and executes the original graph, including failed and previously unseen
//! units. Only successful metadata compilations with an unchanged, closed input snapshot can avoid
//! starting rustc. Replaying rustc's streams through Cargo preserves Cargo diagnostic attribution.

use std::collections::BTreeMap;
use std::ffi::OsString;
#[cfg(not(test))]
use std::fs;
use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};
#[cfg(not(test))]
use std::{env as capture_env, io};

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use tests::snapshot_io as fs;
#[cfg(test)]
pub(super) use tests::snapshot_io;
#[cfg(test)]
use tests::{process_io::io, snapshot_io::environment as capture_env};

use super::rustc_wrapper::{CAPTURE_DIR_VAR, ORIGINAL_WRAPPER_VAR, RustcInvocation, parse_invocation};

pub(super) const REPLAY_VAR: &str = "CARGO_GAMMA_RUSTC_REPLAY";
pub(super) const ROOT_VAR: &str = "CARGO_GAMMA_RUSTC_REPLAY_ROOT";
pub(super) const TARGET_VAR: &str = "CARGO_GAMMA_RUSTC_REPLAY_TARGET";
/// Cargo writes to a pipe; the display truncates this fixed width to the actual terminal width.
pub(super) const PROGRESS_WIDTH: usize = 100;
const MAX_FILES: usize = 100_000;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_STREAM_BYTES: usize = 4 * 1024 * 1024;
mod runtime;
pub(super) const RUNTIME_VAR: &str = "CARGO_GAMMA_RUSTC_REPLAY_RUNTIME";
pub(super) const COMMAND_VAR: &str = "CARGO_GAMMA_RUSTC_COMMAND";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Record {
    context: [u8; 32],
    inputs: Vec<(Utf8PathBuf, [u8; 32])>,
    outputs: Vec<(Utf8PathBuf, [u8; 32])>,
    inventory: [u8; 32],
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    compiler: Utf8PathBuf,
    source: Utf8PathBuf,
    externs: Vec<Utf8PathBuf>,
    searches: Vec<(Utf8PathBuf, [u8; 32])>,
}

pub(super) fn unambiguous_path(path: &Utf8Path) -> bool {
    unambiguous_path_with(path, |path| {
        fs::symlink_metadata(path).map(|metadata| metadata.file_type().is_symlink())
    })
}

fn unambiguous_path_with(path: &Utf8Path, mut symlink: impl FnMut(&Utf8Path) -> io::Result<bool>) -> bool {
    path.ancestors().all(|path| match symlink(path) {
        Ok(symlink) => !symlink,
        Err(cause) => cause.kind() == io::ErrorKind::NotFound,
    })
}

/// Fingerprints the complete UTF-8 command and environment without storing environment values.
///
/// The caller must separately account for files, toolchain contents, and dynamic dependencies.
/// This is deliberately not a claim that a Cargo invocation has a complete cacheable context.
pub(super) fn command_fingerprint(command: &Command, inherited: impl IntoIterator<Item = (OsString, OsString)>) -> Option<[u8; 32]> {
    let mut environment = BTreeMap::new();
    for (name, value) in inherited {
        environment.insert(name.into_string().ok()?, value.into_string().ok()?);
    }
    for (name, value) in command.get_envs() {
        let name = name.to_str()?.to_owned();
        if let Some(value) = value {
            environment.insert(name, value.to_str()?.to_owned());
        } else {
            let _removed = environment.remove(&name);
        }
    }
    let mut hash = blake3::Hasher::new();
    // #[gamma::skip(call_result.default, tag = "equivalent", reason = "This call returns (); evaluating it before replacing () with its default preserves every side effect.")]
    part(&mut hash, command.get_program().to_str()?.as_bytes());
    // #[gamma::skip(call_result.default, tag = "equivalent", reason = "This call returns (); evaluating it before replacing () with its default preserves every side effect.")]
    part(&mut hash, command.get_current_dir()?.to_str()?.as_bytes());
    // #[gamma::skip(call.replace_with_default, stmt.delete_call, tag = "equivalent", reason = "Length-prefixed arguments and the NUL-bearing native environment count already delimit the argument boundary; exact digest bytes are not a public contract.")]
    // #[gamma::skip(call_result.default, tag = "equivalent", reason = "This call returns (); evaluating it before replacing () with its default preserves every side effect.")]
    part(&mut hash, &(command.get_args().count() as u64).to_le_bytes());
    for argument in command.get_args() {
        // #[gamma::skip(call_result.default, tag = "equivalent", reason = "This call returns (); evaluating it before replacing () with its default preserves every side effect.")]
        part(&mut hash, argument.to_str()?.as_bytes());
    }
    // #[gamma::skip(call.replace_with_default, stmt.delete_call, tag = "equivalent", reason = "The argument count and length-prefixed name/value pairs already delimit the environment; this fixed marker only changes opaque digest encoding.")]
    // #[gamma::skip(call_result.default, tag = "equivalent", reason = "This call returns (); evaluating it before replacing () with its default preserves every side effect.")]
    part(&mut hash, b"environment");
    // #[gamma::skip(call.replace_with_default, stmt.delete_call, tag = "equivalent", reason = "The explicit argument count and length-prefixed environment pairs already delimit all fields without this count.")]
    // #[gamma::skip(call_result.default, tag = "equivalent", reason = "This call returns (); evaluating it before replacing () with its default preserves every side effect.")]
    part(&mut hash, &(environment.len() as u64).to_le_bytes());
    for (name, value) in environment {
        // #[gamma::skip(call_result.default, tag = "equivalent", reason = "This call returns (); evaluating it before replacing () with its default preserves every side effect.")]
        part(&mut hash, name.as_bytes());
        // #[gamma::skip(call_result.default, tag = "equivalent", reason = "This call returns (); evaluating it before replacing () with its default preserves every side effect.")]
        part(&mut hash, value.as_bytes());
    }
    Some(*hash.finalize().as_bytes())
}

fn part(hash: &mut blake3::Hasher, value: &[u8]) {
    let _hash = hash.update(&(value.len() as u64).to_le_bytes()).update(value);
}

trait Files {
    fn read(&self, path: &Utf8Path) -> Option<Vec<u8>>;
    /// Returns every regular file recursively, declining symlinks and unreadable entries.
    fn files(&self, root: &Utf8Path, excluded: &Utf8Path) -> Option<Vec<Utf8PathBuf>>;
}

struct NativeFiles;

pub(super) fn read_snapshot(path: impl AsRef<Utf8Path>) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path.as_ref())?;
    let length = file.metadata()?.len();
    read_snapshot_with(file, length, MAX_FILE_BYTES)
}

fn read_snapshot_with(reader: impl io::Read, length: u64, limit: u64) -> io::Result<Vec<u8>> {
    if length > limit {
        return Err(io::Error::other("compiler snapshot file exceeds its capture limit"));
    }
    // #[gamma::skip(expr.increment, literal.int_increment, tag = "equivalent", reason = "This only changes Vec preallocation; read_to_end grows it and the same length limit validates the bytes.")]
    let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    let _read = reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("compiler snapshot file grew beyond its capture limit"));
    }
    Ok(bytes)
}

pub(super) fn snapshot_paths(root: &Utf8Path, excluded: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
    NativeFiles.files(root, excluded)
}

/// Implicit `extern crate` discovery is not represented by Cargo's explicit extern arguments.
/// Nightly source extensions are likewise outside this closed capture protocol.
pub(super) fn source_tokens_closed(tokens: proc_macro2::TokenStream) -> bool {
    let mut tokens = tokens.into_iter().peekable();
    while let Some(token) = tokens.next() {
        match token {
            proc_macro2::TokenTree::Ident(ident)
                if ident == "extern"
                    || ident.to_string().starts_with("rustc_")
                    || (ident == "feature"
                        && matches!(tokens.peek(), Some(proc_macro2::TokenTree::Group(group)) if group.delimiter() == proc_macro2::Delimiter::Parenthesis)) =>
            {
                return false;
            }
            proc_macro2::TokenTree::Group(group) if !source_tokens_closed(group.stream()) => return false,
            _ => {}
        }
    }
    true
}

impl Files for NativeFiles {
    fn read(&self, path: &Utf8Path) -> Option<Vec<u8>> {
        read_snapshot(path).ok()
    }

    fn files(&self, root: &Utf8Path, excluded: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
        if !unambiguous_path(root) {
            return None;
        }
        collect_files(
            root,
            excluded,
            MAX_FILES,
            |directory| fs::read_dir(directory).ok(),
            |entry| {
                let entry = entry.ok()?;
                Some((Utf8PathBuf::from_path_buf(entry.path()).ok()?, entry))
            },
            |entry| {
                let kind = entry.file_type().ok()?;
                Some(
                    if
                    // #[gamma::skip(cond.always_false, tag = "equivalent", reason = "A native symlink is neither a directory nor a regular file, so the final Other branch still declines it.")]
                    kind.is_symlink() {
                        SnapshotKind::Symlink
                    } else if kind.is_dir() {
                        SnapshotKind::Directory
                    } else if kind.is_file() {
                        SnapshotKind::File
                    } else {
                        SnapshotKind::Other
                    },
                )
            },
        )
    }
}

enum SnapshotKind {
    Directory,
    File,
    Symlink,
    Other,
}

fn collect_files<E, I: IntoIterator>(
    root: &Utf8Path,
    excluded: &Utf8Path,
    limit: usize,
    mut read_directory: impl FnMut(&Utf8Path) -> Option<I>,
    mut entry_path: impl FnMut(I::Item) -> Option<(Utf8PathBuf, E)>,
    mut entry_kind: impl FnMut(E) -> Option<SnapshotKind>,
) -> Option<Vec<Utf8PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_owned()];
    let mut visited = 0_usize;
    while let Some(directory) = pending.pop() {
        for entry in read_directory(&directory)? {
            visited += 1;
            if visited > limit {
                return None;
            }
            let (path, entry) = entry_path(entry)?;
            if !excluded.as_str().is_empty() && path.starts_with(excluded) {
                continue;
            }
            match entry_kind(entry)? {
                SnapshotKind::Symlink | SnapshotKind::Other => return None,
                SnapshotKind::Directory => pending.push(path),
                SnapshotKind::File => files.push(path),
            }
        }
    }
    files.sort();
    Some(files)
}

fn digest(files: &impl Files, path: &Utf8Path) -> Option<[u8; 32]> {
    Some(*blake3::hash(&files.read(path)?).as_bytes())
}

fn absolute(path: &Utf8Path, cwd: &Utf8Path) -> Utf8PathBuf {
    // #[gamma::skip(cond.always_false, tag = "equivalent", reason = "Joining an absolute path replaces the base, so cwd.join also returns the same absolute path.")]
    if path.is_absolute() { path.to_owned() } else { cwd.join(path) }
}

fn inventory(files: &impl Files, root: &Utf8Path, target: &Utf8Path) -> Option<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    for path in files.files(root, target)? {
        // #[gamma::skip(call_result.default, tag = "equivalent", reason = "This call returns (); evaluating it before replacing () with its default preserves every side effect.")]
        part(&mut hash, path.as_str().as_bytes());
    }
    Some(*hash.finalize().as_bytes())
}

fn eligible(args: &[OsString], invocation: &RustcInvocation, environment: &BTreeMap<String, String>) -> bool {
    !invocation.opaque_extern
        && !invocation.crate_types.iter().any(|kind| kind == "proc-macro")
        && invocation.crate_name != "build_script_build"
        && !environment.contains_key("OUT_DIR")
        && !environment.contains_key(ORIGINAL_WRAPPER_VAR)
        && !environment.contains_key("RUSTC_WORKSPACE_WRAPPER")
        && !environment.keys().any(|name| name.starts_with("CARGO_BIN_EXE_"))
        && invocation
            .externs
            .iter()
            .all(|path| matches!(path.extension(), Some("rlib" | "rmeta")))
        && metadata_arguments(args).is_some()
}

/// Unknown rustc options can introduce additional files or executable plugins.
fn metadata_arguments(args: &[OsString]) -> Option<Vec<Utf8PathBuf>> {
    let mut args = args.iter().map(|arg| arg.to_str()).collect::<Option<Vec<_>>>()?.into_iter();
    let mut metadata = false;
    let mut source = false;
    let mut searches = Vec::new();
    while let Some(argument) = args.next() {
        if argument == "--emit=dep-info,metadata" {
            metadata = true;
        } else if argument == "--test" {
        } else if argument == "-C" {
            codegen_option(args.next()?)?;
        } else if let Some(value) = argument.strip_prefix("-C") {
            codegen_option(value)?;
        } else if argument == "-L" {
            searches.push(args.next()?.strip_prefix("dependency=")?.into());
        } else if let Some(value) = argument.strip_prefix("-Ldependency=") {
            searches.push(value.into());
        } else if argument == "--target" {
            builtin_target(args.next()?)?;
        } else if let Some(value) = argument.strip_prefix("--target=") {
            builtin_target(value)?;
        } else if
        // #[gamma::skip(collection.reverse_array, tag = "equivalent", reason = "The recognized option array is used only for pure membership testing.")]
        [
            "--crate-name",
            "--crate-type",
            "--edition",
            "--error-format",
            "--json",
            "--diagnostic-width",
            "--cfg",
            "--check-cfg",
            "--cap-lints",
            "--extern",
            "--out-dir",
            "--allow",
            "--warn",
            "--deny",
            "--forbid",
            "--force-warn",
        ]
        .contains(&argument)
        {
            let _value = args.next()?;
        } else if
        // #[gamma::skip(collection.reverse_array, tag = "equivalent", reason = "The recognized option prefixes are tested only by a pure any predicate.")]
        [
            "--crate-name=",
            "--crate-type=",
            "--edition=",
            "--error-format=",
            "--json=",
            "--diagnostic-width=",
            "--cfg=",
            "--check-cfg=",
            "--cap-lints=",
            "--extern=",
            "--out-dir=",
            "--allow=",
            "--warn=",
            "--deny=",
            "--forbid=",
            "--force-warn=",
        ]
        .iter()
        .any(|prefix| argument.starts_with(prefix))
        {
        } else if !argument.starts_with('-') && Utf8Path::new(argument).extension() == Some("rs") && !source {
            source = true;
        } else {
            return None;
        }
    }
    (metadata && source).then_some(searches)
}

fn codegen_option(value: &str) -> Option<()> {
    let (name, value) = value.split_once('=')?;
    (
        // #[gamma::skip(collection.reverse_array, tag = "equivalent", reason = "Recognized codegen names are tested only by pure membership.")]
        [
            "metadata",
            "extra-filename",
            "opt-level",
            "embed-bitcode",
            "debuginfo",
            "incremental",
            "strip",
            "debug-assertions",
            "overflow-checks",
            "panic",
            "codegen-units",
        ]
        .contains(&name)
            && !value.is_empty()
    )
    .then_some(())
}

fn builtin_target(value: &str) -> Option<()> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')).then_some(())
}

fn dep_paths(
    text: &str,
    invocation: &RustcInvocation,
    cwd: &Utf8Path,
    root: &Utf8Path,
    target: &Utf8Path,
) -> Option<(Vec<Utf8PathBuf>, Vec<Utf8PathBuf>)> {
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        if line.starts_with("# env-dep:") {
            continue; // The complete compiler environment is already fingerprinted.
        }
        let Some((output, dependencies)) = line.split_once(": ") else {
            if line.ends_with(':') {
                continue; // rustc's empty phony dependency rules.
            }
            return None;
        };
        let output_paths = super::build::messages::dependencies(output);
        if output_paths.len() != 1 {
            return None;
        }
        let output = absolute(Utf8Path::new(&output_paths[0]), cwd);
        if !output.starts_with(target) || output.components().any(|component| component.as_str() == "..") {
            return None;
        }
        outputs.push(output);
        for path in super::build::messages::dependencies(dependencies) {
            let path = absolute(Utf8Path::new(&path), cwd);
            if !path.starts_with(root) || path.components().any(|component| component.as_str() == "..") {
                return None;
            }
            inputs.push(path);
        }
    }
    inputs.sort();
    inputs.dedup();
    outputs.sort();
    outputs.dedup();
    (inputs.contains(&absolute(&invocation.source, cwd)) && outputs.iter().any(|path| path.extension() == Some("rmeta")))
        .then_some((inputs, outputs))
}

struct Unit<'a> {
    context: [u8; 32],
    invocation: &'a RustcInvocation,
    cwd: &'a Utf8Path,
    root: &'a Utf8Path,
    target: &'a Utf8Path,
    compiler: &'a Utf8Path,
    searches: &'a [Utf8PathBuf],
    captures: &'a Utf8Path,
    runtime: Option<&'a Utf8Path>,
}

impl Unit<'_> {
    fn snapshot(&self, files: &impl Files, stdout: Vec<u8>, stderr: Vec<u8>) -> Option<Record> {
        let dependencies = closed_dependencies(
            files,
            self.invocation,
            self.captures,
            self.root,
            self.target,
            self.runtime,
            self.compiler,
        )?;
        let dep = absolute(
            &self
                .invocation
                .out_dir
                .join(format!("{}{}.d", self.invocation.crate_name, self.invocation.extra_filename)),
            self.cwd,
        );
        let text = String::from_utf8(files.read(&dep)?).ok()?;
        let (mut inputs, mut outputs) = dep_paths(&text, self.invocation, self.cwd, self.root, self.target)?;
        for path in &inputs {
            let tokens = String::from_utf8(files.read(path)?).ok()?.parse().ok()?;
            if !source_tokens_closed(tokens) {
                return None;
            }
        }
        outputs.push(dep);
        inputs.push(self.compiler.to_owned());
        // A rustup shim, custom sysroot, or nonstandard compiler layout is not a closed toolchain.
        let bin = self.compiler.parent()?;
        if bin.file_name()? != "bin"
            ||
            // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The preceding bin-name check excludes a filesystem root compiler path, so the compiler has a filename and stem.")]
            self.compiler.file_stem()? != "rustc"
            || !self.compiler.is_absolute()
        {
            return None;
        }
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The preceding guard established the normal directory name bin, which necessarily has a parent.")]
        let toolchain = bin.parent()?;
        let libraries = files.files(&toolchain.join("lib"), self.target)?;
        if libraries.is_empty() {
            return None;
        }
        inputs.extend(libraries);
        inputs.extend(files.files(bin, self.target)?);
        inputs.extend(dependencies);
        // #[gamma::skip(arith.add_to_mul, tag = "equivalent", reason = "This only changes Vec preallocation; push grows the vector without changing the collected search paths.")]
        let mut searches = Vec::with_capacity(self.invocation.externs.len() + self.searches.len());
        for path in &self.invocation.externs {
            let path = absolute(path, self.cwd);
            // #[gamma::skip(logical.or_remove_left, tag = "equivalent", reason = "closed_dependencies already required every extern to be absolute and target-contained before this snapshot step.")]
            if !path.starts_with(self.target) || path.components().any(|component| component.as_str() == "..") {
                return None;
            }
            let bytes = files.read(&path)?;
            // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "eligible already checked that every extern has an rmeta or rlib extension.")]
            if !match path.extension()? {
                "rmeta" => bytes.starts_with(b"rust\0"),
                "rlib" => bytes.starts_with(b"!<arch>\n"),
                // #[gamma::skip(literal.bool_flip, tag = "equivalent", reason = "eligible permits only rmeta and rlib extern extensions, so this wildcard is unreachable.")]
                _ => false,
            } {
                return None;
            }
            // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "An absolute extern with an rmeta or rlib filename necessarily has a parent.")]
            searches.push(path.parent()?.to_owned());
        }
        for directory in self.searches {
            let directory = absolute(directory, self.cwd);
            if !directory.starts_with(self.target) || directory.components().any(|component| component.as_str() == "..") {
                return None;
            }
            searches.push(directory);
        }
        // #[gamma::skip(iter.remove_sort, tag = "equivalent", reason = "Searches are constructed deterministically for the same command; capture and revalidation retain the same order.")]
        searches.sort();
        // #[gamma::skip(iter.remove_dedup, tag = "equivalent", reason = "Retaining the same duplicate search stamps in capture and revalidation preserves record equality.")]
        searches.dedup();
        let searches = searches
            .into_iter()
            .map(|directory| {
                let shape = inventory(files, &directory, self.captures)?;
                Some((directory, shape))
            })
            .collect::<Option<Vec<_>>>()?;
        // #[gamma::skip(iter.remove_sort, tag = "equivalent", reason = "Dep-info and native toolchain inventories are deterministic for the same command, preserving capture and replay equality.")]
        inputs.sort();
        // #[gamma::skip(iter.remove_dedup, tag = "equivalent", reason = "Retaining identical duplicate input stamps on both capture and revalidation preserves record equality.")]
        inputs.dedup();
        let stamps = |paths: Vec<Utf8PathBuf>| -> Option<Vec<_>> {
            paths.into_iter().map(|path| Some((path.clone(), digest(files, &path)?))).collect()
        };
        Some(Record {
            context: self.context,
            inputs: stamps(inputs)?,
            outputs: stamps(outputs)?,
            inventory: inventory(files, self.root, self.target)?,
            stdout,
            stderr,
            compiler: self.compiler.to_owned(),
            source: absolute(&self.invocation.source, self.cwd),
            externs: self.invocation.externs.iter().map(|path| absolute(path, self.cwd)).collect(),
            searches,
        })
    }
}

pub(super) struct SourceContext {
    pub(super) toolchain: Vec<(Utf8PathBuf, [u8; 32])>,
    pub(super) units: Vec<[u8; 32]>,
    pub(super) runtime: Option<Utf8PathBuf>,
}

/// Describes a closed source-only Cargo check without treating mutable unit outputs as inputs.
pub(super) fn source_only_context(
    directory: &Utf8Path,
    stdout: &str,
    root: &Utf8Path,
    target: &Utf8Path,
    runtime: &Utf8Path,
) -> Option<SourceContext> {
    source_only_context_with(&NativeFiles, directory, stdout, root, target, Some(runtime))
}

fn source_only_context_with(
    files: &impl Files,
    directory: &Utf8Path,
    stdout: &str,
    root: &Utf8Path,
    target: &Utf8Path,
    runtime_root: Option<&Utf8Path>,
) -> Option<SourceContext> {
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Prepared capture generations are owned under capture-root/replay/digest, so both parents exist structurally.")]
    let runtime = runtime::load(files, directory.parent()?.parent()?)
        .filter(|runtime| runtime_root.is_some_and(|root| runtime.valid(files, root, None)));
    let mut records = Vec::new();
    // #[gamma::skip(literal.str_to_xyzzy, tag = "equivalent", reason = "Native inventories contain absolute paths, none of which starts with the relative normal component xyzzy.")]
    for path in files.files(directory, Utf8Path::new(""))? {
        let record: Record = serde_json::from_slice(&files.read(&path)?).ok()?;
        if record
            .externs
            .iter()
            .any(|artifact| !runtime.as_ref().is_some_and(|runtime| runtime.contains(files, artifact)))
            || !record.source.starts_with(root)
        {
            return None;
        }
        if !record_current(files, &record, root, target, directory)? {
            continue;
        }
        records.push(record);
    }
    let mut expected = Vec::new();
    for line in stdout.lines() {
        let message: serde_json::Value = serde_json::from_str(line).ok()?;
        match message.get("reason")?.as_str()? {
            "compiler-artifact" => {
                for path in message.get("filenames")?.as_array()? {
                    let path = Utf8PathBuf::from(path.as_str()?);
                    if path.extension() != Some("rmeta") {
                        return None;
                    }
                    expected.push(path);
                }
            }
            "build-script-executed" => return None,
            _ => {}
        }
    }
    // #[gamma::skip(logical.or_remove_right, tag = "equivalent", reason = "With nonempty expected outputs, the following lookup in an empty records vector independently declines.")]
    if expected.is_empty() || records.is_empty() {
        return None;
    }
    for output in expected {
        if runtime.as_ref().is_some_and(|runtime| runtime.contains(files, &output)) {
            continue;
        }
        if !records.iter().any(|record| {
            record
                .outputs
                .iter()
                .any(|(path, hash)| path == &output && digest(files, path) == Some(*hash))
        }) {
            return None;
        }
    }
    let mut stamps = Vec::new();
    let mut contexts = Vec::new();
    if let Some(runtime) = &runtime {
        stamps.extend(runtime.files.iter().cloned());
        stamps.extend(runtime.artifacts.iter().cloned());
        contexts.push(runtime.context);
    }
    for record in records {
        contexts.push(record.context);
        for (path, hash) in record.inputs {
            if !path.starts_with(root)
                && (!path.starts_with(target)
                    || runtime
                        .as_ref()
                        .is_some_and(|runtime| runtime.artifacts.iter().any(|(artifact, _)| artifact == &path)))
            {
                if digest(files, &path) != Some(hash) {
                    return None;
                }
                stamps.push((path, hash));
            }
        }
        stamps.push((record.compiler.clone(), digest(files, &record.compiler)?));
    }
    stamps.sort();
    stamps.dedup();
    contexts.sort_unstable();
    contexts.dedup();
    Some(SourceContext {
        toolchain: stamps,
        units: contexts,
        runtime: runtime.map(|runtime| runtime.root),
    })
}

fn record_current(files: &impl Files, record: &Record, root: &Utf8Path, target: &Utf8Path, directory: &Utf8Path) -> Option<bool> {
    if inventory(files, root, target) != Some(record.inventory)
        || !record.inputs.iter().any(|(path, _)| path == &record.source)
        || !record.inputs.iter().any(|(path, _)| path == &record.compiler)
        || record
            .searches
            .iter()
            .any(|(search, expected)| inventory(files, search, directory) != Some(*expected))
        || record
            .inputs
            .iter()
            .chain(&record.outputs)
            .filter(|(path, _)| !path.starts_with(target) || record.outputs.iter().any(|(output, _)| output == path))
            .any(|(path, expected)| digest(files, path) != Some(*expected))
    {
        return Some(false);
    }
    let bin = record.compiler.parent()?;
    if bin.file_name()? != "bin"
        ||
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The preceding bin-name check excludes a filesystem root compiler path, so the compiler has a filename and stem.")]
        record.compiler.file_stem()? != "rustc"
    {
        return None;
    }
    let mut toolchain = files.files(bin, target)?;
    toolchain.extend(files.files(
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The preceding guard established the normal directory name bin, which necessarily has a parent.")]
        &bin.parent()?.join("lib"),
        target,
    )?);
    Some(
        !toolchain.is_empty()
            && toolchain.iter().all(|path| {
                record
                    .inputs
                    .iter()
                    .any(|(input, hash)| input == path && digest(files, path) == Some(*hash))
            }),
    )
}

/// Artifact suffixes alone cannot exclude procedural macro reexports. Every transitive extern
/// must have a successful, closed capture from this exact Cargo-command generation.
fn closed_dependencies(
    files: &impl Files,
    invocation: &RustcInvocation,
    directory: &Utf8Path,
    root: &Utf8Path,
    target: &Utf8Path,
    runtime_root: Option<&Utf8Path>,
    compiler: &Utf8Path,
) -> Option<Vec<Utf8PathBuf>> {
    if invocation.externs.is_empty() {
        return Some(Vec::new());
    }
    let records = files
        .files(directory, Utf8Path::new(""))? // #[gamma::skip(literal.str_to_xyzzy, tag = "equivalent", reason = "Native inventories contain absolute paths, none of which starts with the relative normal component xyzzy.")]
        .into_iter()
        .filter(|path| path.extension() == Some("json"))
        .map(|path| serde_json::from_slice::<Record>(&files.read(&path)?).ok())
        .collect::<Option<Vec<_>>>()?;
    let mut pending = invocation.externs.clone();
    let mut visited = std::collections::BTreeSet::new();
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Prepared capture generations are owned under capture-root/replay/digest, so both parents exist structurally.")]
    let runtime = runtime::load(files, directory.parent()?.parent()?)
        .filter(|runtime| runtime_root.is_some_and(|root| runtime.valid(files, root, Some(compiler))));
    let mut additional = Vec::new();
    while let Some(artifact) = pending.pop() {
        // #[gamma::skip(logical.or_remove_left, tag = "equivalent", reason = "Prepared targets are absolute, so the remaining target-prefix check independently rejects relative artifacts.")]
        if !artifact.is_absolute() || !artifact.starts_with(target) {
            return None;
        }
        if !visited.insert(artifact.clone()) {
            continue;
        }
        if let Some(runtime) = &runtime
            && runtime.contains(files, &artifact)
        {
            additional.extend(runtime.files.iter().map(|(path, _)| path.clone()));
            continue;
        }
        let expected = digest(files, &artifact)?;
        let record = records.iter().find(|record| {
            record.outputs.iter().any(|(path, hash)| {
                // #[gamma::skip(logical.and_remove_right, tag = "equivalent", reason = "The later all-inputs-and-outputs digest check independently verifies this same matching artifact hash.")]
                path == &artifact && *hash == expected
            }) && record.source.starts_with(root)
                && record.inputs.iter().any(|(path, _)| path == &record.source)
                && record.inputs.iter().any(|(path, _)| path == &record.compiler)
                && record
                    .inputs
                    .iter()
                    .chain(&record.outputs)
                    .all(|(path, hash)| digest(files, path) == Some(*hash))
                && record
                    .searches
                    .iter()
                    .all(|(path, hash)| inventory(files, path, directory) == Some(*hash))
                && inventory(files, root, target) == Some(record.inventory)
        })?;
        pending.extend(record.externs.iter().cloned());
    }
    additional.extend(visited);
    // #[gamma::skip(iter.remove_sort, tag = "equivalent", reason = "Unit::snapshot sorts the complete input vector again before constructing its Record.")]
    additional.sort();
    // #[gamma::skip(iter.remove_dedup, tag = "equivalent", reason = "Unit::snapshot deduplicates the complete input vector again before constructing its Record.")]
    additional.dedup();
    Some(additional)
}

impl Unit<'_> {
    fn reusable(&self, files: &impl Files, record: &Record) -> bool {
        if record.context != self.context {
            return false;
        }
        self.snapshot(files, Vec::new(), Vec::new()).is_some_and(|current| {
            current.inventory == record.inventory
                && current.inputs == record.inputs
                && current.outputs == record.outputs
                && current.searches == record.searches
        })
    }
}

fn stable_inputs(before: &Record, after: &Record) -> bool {
    let outputs: std::collections::BTreeSet<_> = before.outputs.iter().chain(&after.outputs).map(|(path, _)| path).collect();
    let inputs = |record: &Record| {
        record
            .inputs
            .iter()
            .filter(|(path, _)| !outputs.contains(path))
            .cloned()
            .collect::<Vec<_>>()
    };
    before.inventory == after.inventory
        && before.searches == after.searches
        && inputs(before) == inputs(after)
        && before
            .outputs
            .iter()
            .map(|(path, _)| path)
            .eq(after.outputs.iter().map(|(path, _)| path))
}

struct CompilerOutput {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    capturable: bool,
    forwarded: bool,
}

fn execute(
    unit: &Unit<'_>,
    files: &impl Files,
    previous: Option<&Record>,
    run: impl FnOnce() -> io::Result<CompilerOutput>,
    publish: impl FnOnce(&Record),
    mut relay: impl FnMut(&[u8], &[u8]) -> io::Result<()>,
) -> io::Result<Option<i32>> {
    if let Some(record) = previous
        && unit.reusable(files, record)
    {
        relay(&record.stdout, &record.stderr)?;
        return Ok(Some(0));
    }
    // Missing or changed dep-info cannot establish a closed capture until a later invocation has
    // observed the resulting graph both before and after compilation.
    let before = unit.snapshot(files, Vec::new(), Vec::new());
    let output = run()?;
    if !output.forwarded {
        relay(&output.stdout, &output.stderr)?;
    }
    if output.code == Some(0)
        && output.capturable
        && let Some(record) = unit.snapshot(files, output.stdout, output.stderr)
        && before.as_ref().is_some_and(|before| stable_inputs(before, &record))
    {
        publish(&record);
    }
    Ok(output.code)
}

/// Uses the wrapper's existing, Cargo-contained child-process boundary.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn compiler_status(command: &mut Command) -> io::Result<Option<i32>> {
    #[cfg(test)]
    let command = &mut tests::process_io::CommandRef(command);
    let environment: BTreeMap<String, String> = match capture_env::vars_os()
        .map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect::<Option<_>>()
    {
        Some(environment) => environment,
        None => return command.status().map(|status| status.code()),
    };
    let Some((directory, unit)) = prepare(command, &environment) else {
        let code = command.status()?.code();
        if code == Some(0) {
            let _provenance = runtime::capture(command, &environment);
        }
        return Ok(code);
    };
    let path = directory.join(format!("{}.json", blake3::Hash::from(unit.context).to_hex()));
    let previous = NativeFiles
        .read(&path)
        .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok());
    let result = execute(
        &unit.borrowed(),
        &NativeFiles,
        previous.as_ref(),
        || capture_output(command),
        |record| {
            if let Ok(bytes) = serde_json::to_vec(record) {
                // An interrupted write is a cache miss, never a successful compiler result.
                let _published = fs::create_dir_all(&directory).and_then(|()| fs::write(&path, bytes));
            }
        },
        |stdout, stderr| {
            io::stdout().write_all(stdout)?;
            io::stderr().write_all(stderr)
        },
    );
    if result.as_ref().is_ok_and(|code| *code == Some(0)) {
        let _provenance = runtime::capture(command, &environment);
    }
    result
}

/// The compiler remains a descendant of the original contained Cargo process. Readers forward live
/// output so Cargo's existing output limits and diagnostic attribution remain in charge.
#[cfg_attr(coverage_nightly, coverage(off))]
fn capture_output(command: &mut Command) -> io::Result<CompilerOutput> {
    #[cfg(test)]
    let command = &mut tests::process_io::CommandRef(command);
    let _command = command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("stdout was explicitly configured as piped above");
    let stderr = child.stderr.take().expect("stderr was explicitly configured as piped above");
    let readers: [Box<dyn io::Read + Send>; 2] = [Box::new(stdout), Box::new(stderr)];
    let result = std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::channel();
        for (index, reader) in readers.into_iter().enumerate() {
            let sender = sender.clone();
            #[cfg(test)]
            let script = tests::process_io::current();
            if let Err(cause) = std::thread::Builder::new().spawn_scoped(scope, move || {
                #[cfg(test)]
                let _scope = tests::process_io::enter(script);
                let result = if index == 0 {
                    capture_stream(reader, io::stdout(), MAX_STREAM_BYTES)
                } else {
                    capture_stream(reader, io::stderr(), MAX_STREAM_BYTES)
                };
                let _sent = sender.send((index, result));
            }) {
                let _killed = child.kill();
                let _waited = child.wait();
                return Err(cause);
            }
        }
        drop(sender);
        let mut streams = [None, None];
        let mut failure = None;
        for _ in 0..2 {
            match receiver.recv() {
                Ok((index, Ok(bytes))) => streams[index] = bytes,
                Ok((_, Err(cause))) => {
                    let _first = failure.get_or_insert(cause);
                    let _killed = child.kill();
                }
                Err(cause) => {
                    let _first = failure.get_or_insert_with(|| io::Error::other(cause));
                    let _killed = child.kill();
                    // #[gamma::skip(loop.break_to_continue, loop.delete_break, tag = "equivalent", reason = "Both spawned readers send exactly once before disconnect; spawn errors return earlier and reader panics propagate from thread::scope.")]
                    break;
                }
            }
        }
        let code = child.wait()?.code();
        if let Some(failure) = failure {
            return Err(failure);
        }
        Ok(CompilerOutput {
            code,
            capturable: streams.iter().all(Option::is_some),
            stdout: streams[0].take().unwrap_or_default(),
            stderr: streams[1].take().unwrap_or_default(),
            forwarded: true,
        })
    });
    let _command = command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    result
}

fn capture_stream(mut reader: impl io::Read, mut writer: impl io::Write, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Some(Vec::new());
    // #[gamma::skip(literal.int_increment, tag = "equivalent", reason = "Read fills the returned prefix, and only that initialized prefix is consumed.")]
    let mut buffer = [0_u8; 16 * 1024];
    let mut failure = None;
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(cause) if cause.kind() == io::ErrorKind::Interrupted => continue,
            Err(cause) => return Err(cause),
        };
        if let Some(captured) = &mut bytes {
            if captured.len().saturating_add(count) <= limit {
                captured.extend_from_slice(&buffer[..count]);
            } else {
                bytes = None;
            }
        }
        if failure.is_none()
            && let Err(cause) = writer.write_all(&buffer[..count])
        {
            failure = Some(cause);
        }
    }
    failure.map_or(Ok(bytes), Err)
}

fn prepare(command: &Command, environment: &BTreeMap<String, String>) -> Option<(Utf8PathBuf, OwnedUnit)> {
    let cwd = Utf8PathBuf::from_path_buf(capture_env::current_dir().ok()?).ok()?;
    prepare_with(command, environment, &cwd)
}

fn prepare_with(command: &Command, environment: &BTreeMap<String, String>, cwd: &Utf8Path) -> Option<(Utf8PathBuf, OwnedUnit)> {
    let generation = environment.get(REPLAY_VAR)?;
    if generation.len() != 64 || !generation.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let root = Utf8PathBuf::from(environment.get(ROOT_VAR)?);
    let target = Utf8PathBuf::from(environment.get(TARGET_VAR)?);
    let directory = Utf8PathBuf::from(environment.get(CAPTURE_DIR_VAR)?).join("replay").join(generation);
    let compiler = Utf8PathBuf::from(command.get_program().to_str()?);
    let args: Vec<OsString> = command.get_args().map(OsString::from).collect();
    let invocation = parse_invocation(&args)?;
    if !root.is_absolute()
        || !target.is_absolute()
        || root == target
        || !cwd.starts_with(&root)
        || !absolute(&invocation.out_dir, cwd).starts_with(&target)
        || !eligible(&args, &invocation, environment)
    {
        return None;
    }
    let mut identity = Command::new(&compiler);
    let _identity = identity.current_dir(cwd).args(&args);
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "eligible validated the same immutable arguments as UTF-8, while the program, working directory, and environment are already UTF-8 paths and Strings.")]
    let context = command_fingerprint(&identity, environment.iter().map(|(k, v)| (k.into(), v.into())))?;
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "eligible just called metadata_arguments successfully on the same immutable arguments.")]
    let searches = metadata_arguments(&args)?;
    Some((
        directory.clone(),
        OwnedUnit {
            context,
            invocation,
            cwd: cwd.to_owned(),
            root,
            target,
            compiler,
            searches,
            captures: directory,
            runtime: environment.get(RUNTIME_VAR).map(Utf8PathBuf::from),
        },
    ))
}

struct OwnedUnit {
    context: [u8; 32],
    invocation: RustcInvocation,
    cwd: Utf8PathBuf,
    root: Utf8PathBuf,
    target: Utf8PathBuf,
    compiler: Utf8PathBuf,
    searches: Vec<Utf8PathBuf>,
    captures: Utf8PathBuf,
    runtime: Option<Utf8PathBuf>,
}

impl OwnedUnit {
    fn borrowed(&self) -> Unit<'_> {
        Unit {
            context: self.context,
            invocation: &self.invocation,
            cwd: &self.cwd,
            root: &self.root,
            target: &self.target,
            compiler: &self.compiler,
            searches: &self.searches,
            captures: &self.captures,
            runtime: self.runtime.as_deref(),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;
