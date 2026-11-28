// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conservative compiler-result reuse underneath Cargo's scheduler.
//!
//! Cargo still constructs and executes the original graph, including failed and previously unseen
//! units. Only successful metadata compilations with an unchanged, closed input snapshot can avoid
//! starting rustc. Replaying rustc's streams through Cargo preserves Cargo diagnostic attribution.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
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
/// Bounds recursive graph inspection for large workspaces/toolchains; exceeding it declines reuse.
const MAX_FILES: usize = 100_000;
/// Bounds one input's capture cost while admitting large compiler libraries; exhaustion declines reuse.
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum compiler output retained per stream for replay.
///
/// Larger streams are forwarded in full but never published for replay.
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

/// Maps an exact output path to a candidate local capture, not proof it produced that artifact.
///
/// Replay must still validate the record, requested output, and current file stamps.
#[derive(Serialize, Deserialize)]
struct OutputIndex {
    record: String,
    source: Utf8PathBuf,
}

fn output_index(directory: &Utf8Path, output: &Utf8Path) -> Utf8PathBuf {
    directory
        .join("outputs")
        .join(blake3::hash(output.as_str().as_bytes()).to_hex().as_str())
}

fn publish_record(directory: &Utf8Path, path: &Utf8Path, record: &Record) -> io::Result<()> {
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Record serialization contains only UTF-8 paths, strings, integers, byte arrays, vectors, and options.")]
    let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    // Indexes must never lead their target. A missing index deliberately declines dependency reuse;
    // a complete record remains usable even if an auxiliary index cannot be published.
    publish_atomic(path, &bytes)?;
    let index = OutputIndex {
        record:
        // #[gamma::skip(try.propagate_to_unwrap, literal.str_to_empty, literal.str_to_xyzzy, tag = "equivalent", reason = "The caller constructs this path from an owned capture directory and a digest filename.")]
        path
            .file_name()
            .ok_or_else(|| io::Error::other("replay record has no filename"))?
            .to_owned(),
        source: record.source.clone(),
    };
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "OutputIndex serialization contains only UTF-8 paths and strings.")]
    let bytes = serde_json::to_vec(&index).map_err(io::Error::other)?;
    for (output, _) in &record.outputs {
        publish_atomic(&output_index(directory, output), &bytes)?;
    }
    Ok(())
}

fn publish_atomic(path: &Utf8Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(test)]
    if snapshot_io::active() {
        return snapshot_io::write(path, bytes);
    }
    super::rustc_wrapper::publish_atomic(path, bytes)
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
    part(&mut hash, command.get_program().to_str()?.as_bytes());
    part(&mut hash, command.get_current_dir()?.to_str()?.as_bytes());
    // #[gamma::skip(call.replace_with_default, stmt.delete_call, tag = "equivalent", reason = "Length-prefixed arguments and the NUL-bearing native environment count already delimit the argument boundary; exact digest bytes are not a public contract.")]
    part(&mut hash, &(command.get_args().count() as u64).to_le_bytes());
    for argument in command.get_args() {
        part(&mut hash, argument.to_str()?.as_bytes());
    }
    // #[gamma::skip(call.replace_with_default, stmt.delete_call, tag = "equivalent", reason = "The argument count and length-prefixed name/value pairs already delimit the environment; this fixed marker only changes opaque digest encoding.")]
    part(&mut hash, b"environment");
    // #[gamma::skip(call.replace_with_default, stmt.delete_call, tag = "equivalent", reason = "The explicit argument count and length-prefixed environment pairs already delimit all fields without this count.")]
    part(&mut hash, &(environment.len() as u64).to_le_bytes());
    for (name, value) in environment {
        part(&mut hash, name.as_bytes());
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

    fn stamp(&self, path: &Utf8Path) -> Option<FileStamp> {
        Some(FileStamp::of(&self.read(path)?))
    }
}

struct NativeFiles;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct FileStamp {
    digest: [u8; 32],
    prefix: [u8; 8],
    length: u64,
}

impl FileStamp {
    fn of(bytes: &[u8]) -> Self {
        let mut prefix = [0; 8];
        let length = bytes.len().min(prefix.len());
        prefix[..length].copy_from_slice(&bytes[..length]);
        Self {
            digest: *blake3::hash(bytes).as_bytes(),
            prefix,
            length: bytes.len() as u64,
        }
    }
}

#[ohno::error]
#[display("compiler snapshot exceeds its capture limit of {limit} bytes")]
struct SnapshotLimitExceeded {
    limit: u64,
}

fn stamp_reader(reader: impl io::Read, limit: u64) -> io::Result<FileStamp> {
    // #[gamma::skip(literal.int_increment, tag = "equivalent", reason = "The production limit permits checked lookahead; either lookahead rejects oversized files at length greater than limit.")]
    let mut reader = reader.take(
        limit
            .checked_add(1)
            .ok_or_else(|| io::Error::other(SnapshotLimitExceeded::new(limit)))?,
    );
    let mut hasher = blake3::Hasher::new();
    let mut prefix = [0; 8];
    let mut length = 0_u64;
    let mut buffer = [
        // #[gamma::skip(literal.int_increment, tag = "equivalent", reason = "File reads overwrite every buffer byte subsequently included in the prefix or digest.")]
        0; 16 * 1024
    ];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            // #[gamma::skip(loop.break_to_continue, tag = "timeout", reason = "EOF remains EOF; continuing instead of finishing repeats reads forever.")]
            break;
        }
        if
        // #[gamma::skip(relational.lt_to_le, literal.int_increment, tag = "equivalent", reason = "At length eight the prefix copy has count zero, so including that boundary changes no bytes.")]
        length < 8 {
            // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The guard bounds length below eight, which fits usize on every supported target.")]
            let offset = usize::try_from(length).map_err(io::Error::other)?;
            let count = read.min(8 - offset);
            prefix[offset..offset + count].copy_from_slice(&buffer[..count]);
        }
        length += read as u64;
        if length > limit {
            return Err(io::Error::other(SnapshotLimitExceeded::new(limit)));
        }
        let _hash = hasher.update(&buffer[..read]);
    }
    Ok(FileStamp {
        digest: *hasher.finalize().as_bytes(),
        prefix,
        length,
    })
}

type DirectoryListings = BTreeMap<(Utf8PathBuf, Utf8PathBuf), Option<Vec<Utf8PathBuf>>>;

/// One observation only: before- and after-build snapshots must never share these stamps.
struct SnapshotFiles<'a, F> {
    inner: &'a F,
    stamps: RefCell<BTreeMap<Utf8PathBuf, Option<FileStamp>>>,
    listings: RefCell<DirectoryListings>,
}

impl<'a, F: Files> SnapshotFiles<'a, F> {
    fn new(inner: &'a F) -> Self {
        Self {
            inner,
            stamps: RefCell::new(BTreeMap::new()),
            listings: RefCell::new(BTreeMap::new()),
        }
    }
}

impl<F: Files> Files for SnapshotFiles<'_, F> {
    fn read(&self, path: &Utf8Path) -> Option<Vec<u8>> {
        let bytes = self.inner.read(path)?;
        let _previous = self.stamps.borrow_mut().insert(path.to_owned(), Some(FileStamp::of(&bytes)));
        Some(bytes)
    }

    fn stamp(&self, path: &Utf8Path) -> Option<FileStamp> {
        if let Some(stamp) = self.stamps.borrow().get(path) {
            return *stamp;
        }
        let stamp = self.inner.stamp(path);
        let _previous = self.stamps.borrow_mut().insert(path.to_owned(), stamp);
        stamp
    }

    fn files(&self, root: &Utf8Path, excluded: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
        let key = (root.to_owned(), excluded.to_owned());
        if let Some(paths) = self.listings.borrow().get(&key) {
            return paths.clone();
        }
        let paths = self.inner.files(root, excluded);
        let _previous = self.listings.borrow_mut().insert(key, paths.clone());
        paths
    }
}

pub(super) fn read_snapshot(path: impl AsRef<Utf8Path>) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path.as_ref())?;
    let length = file.metadata()?.len();
    read_snapshot_with(file, length, MAX_FILE_BYTES)
}

pub(super) fn digest_snapshot(path: &Utf8Path) -> io::Result<[u8; 32]> {
    snapshot_stamp(path).map(|stamp| stamp.digest)
}

fn snapshot_stamp(path: &Utf8Path) -> io::Result<FileStamp> {
    let file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    snapshot_stamp_with(file, length)
}

fn snapshot_stamp_with(reader: impl io::Read, length: u64) -> io::Result<FileStamp> {
    if length > MAX_FILE_BYTES {
        return Err(io::Error::other(SnapshotLimitExceeded::new(MAX_FILE_BYTES)));
    }
    stamp_reader(reader, MAX_FILE_BYTES)
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

    fn stamp(&self, path: &Utf8Path) -> Option<FileStamp> {
        snapshot_stamp(path).ok()
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
    Some(files.stamp(path)?.digest)
}

fn absolute(path: &Utf8Path, cwd: &Utf8Path) -> Utf8PathBuf {
    // #[gamma::skip(cond.always_false, tag = "equivalent", reason = "Joining an absolute path replaces the base, so cwd.join also returns the same absolute path.")]
    if path.is_absolute() { path.to_owned() } else { cwd.join(path) }
}

fn inventory(files: &impl Files, root: &Utf8Path, target: &Utf8Path) -> Option<[u8; 32]> {
    let mut hash = blake3::Hasher::new();
    for path in files.files(root, target)? {
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
        let snapshot = SnapshotFiles::new(files);
        self.snapshot_with(&snapshot, stdout, stderr)
    }

    fn snapshot_with(&self, files: &impl Files, stdout: Vec<u8>, stderr: Vec<u8>) -> Option<Record> {
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
        let dependencies = closed_dependencies(
            files,
            self.invocation,
            self.captures,
            self.root,
            self.target,
            self.runtime,
            self.compiler,
        )?;
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
            // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Dependency validation already memoized a successful stamp for this extern in the same snapshot.")]
            let stamp = files.stamp(&path)?;
            let bytes = &stamp.prefix[..stamp.length.min(8) as usize];
            // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "eligible already checked that every extern has an rmeta or rlib extension.")]
            if !match path.extension()? {
                "rmeta" => bytes.starts_with(b"rust\0"),
                "rlib" => {
                    // #[gamma::skip(string.starts_with_to_ends_with, tag = "equivalent", reason = "The prefix has at most eight bytes and the archive marker has exactly eight; both predicates are equality.")]
                    bytes.starts_with(b"!<arch>\n")
                }
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
    let expected = metadata_outputs(stdout)?;
    let files = &SnapshotFiles::new(files);
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Prepared capture generations are owned under capture-root/replay/digest, so both parents exist structurally.")]
    let runtime = runtime::load(files, directory.parent()?.parent()?);
    let mut records = Vec::new();
    // #[gamma::skip(literal.str_to_xyzzy, tag = "equivalent", reason = "Native inventories contain absolute paths, none of which starts with the relative normal component xyzzy.")]
    for path in files.files(directory, Utf8Path::new(""))? {
        if path.parent() != Some(directory) || path.extension() != Some("json") {
            continue;
        }
        let record: Record = serde_json::from_slice(&files.read(&path)?).ok()?;
        if
        // #[gamma::skip(logical.or_remove_left, tag = "equivalent", reason = "The later validated-runtime check independently requires every extern's membership and matching digest.")]
        record.externs.iter().any(|artifact| {
            !runtime
                .as_ref()
                .is_some_and(|runtime| runtime.artifacts.iter().any(|(path, _)| path == artifact))
        }) || !record.source.starts_with(root)
        {
            return None;
        }
        records.push(record);
    }
    let runtime = runtime.filter(|runtime| runtime_root.is_some_and(|root| runtime.valid(files, root, None)));
    let mut current = Vec::new();
    for record in records {
        if record
            .externs
            .iter()
            .any(|artifact| !runtime.as_ref().is_some_and(|runtime| runtime.contains(files, artifact)))
        {
            return None;
        }
        if !record_current(files, &record, root, target, directory)? {
            continue;
        }
        current.push(record);
    }
    let records = current;
    if records.is_empty() {
        return None;
    }
    for output in expected {
        if runtime.as_ref().is_some_and(|runtime| runtime.contains(files, &output)) {
            continue;
        }
        if !records.iter().any(|record| {
            record.outputs.iter().any(|(path, hash)|
                    // #[gamma::skip(logical.and_remove_right, tag = "equivalent", reason = "record_current already verified every output digest in this same memoized snapshot.")]
                    path == &output && digest(files, path) == Some(*hash))
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
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "record_current already verified and memoized this compiler input's digest.")]
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

pub(super) fn metadata_outputs(stdout: &str) -> Option<Vec<Utf8PathBuf>> {
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
    (!expected.is_empty()).then_some(expected)
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
    let inputs: BTreeMap<_, _> = record.inputs.iter().map(|(path, hash)| (path, hash)).collect();
    Some(
        !toolchain.is_empty()
            && toolchain
                .iter()
                .all(|path| inputs.get(path).is_some_and(|hash| digest(files, path) == Some(**hash))),
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
    // #[gamma::skip(cond.always_false, tag = "equivalent", reason = "An empty extern list also leaves the traversal queue and final dependency vector empty.")]
    if invocation.externs.is_empty() {
        return Some(Vec::new());
    }
    let mut pending = invocation.externs.clone();
    let mut visited = std::collections::BTreeSet::new();
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Prepared capture generations are owned under capture-root/replay/digest, so both parents exist structurally.")]
    let runtime = runtime::load(files, directory.parent()?.parent()?);
    let mut runtime_valid = None;
    let mut records = BTreeMap::new();
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
            && runtime.artifacts.iter().any(|(path, _)| path == &artifact)
        {
            if !*runtime_valid.get_or_insert_with(|| runtime_root.is_some_and(|root| runtime.valid(files, root, Some(compiler))))
                || !runtime.contains(files, &artifact)
            {
                return None;
            }
            additional.extend(runtime.files.iter().map(|(path, _)| path.clone()));
            continue;
        }
        let index: OutputIndex = serde_json::from_slice(&files.read(&output_index(directory, &artifact))?).ok()?;
        if
        // #[gamma::skip(logical.or_remove_left, tag = "equivalent", reason = "The later record-source root check and equality with index.source enforce the same containment.")]
        !index.source.starts_with(root) || Utf8Path::new(&index.record).file_name() != Some(index.record.as_str()) {
            return None;
        }
        let record = match records.entry(directory.join(index.record)) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let bytes = files.read(entry.key())?;
                entry.insert(serde_json::from_slice::<Record>(&bytes).ok()?)
            }
        };
        if record.source != index.source {
            return None;
        }
        let expected = digest(files, &artifact)?;
        if !((
            // #[gamma::skip(logical.and_remove_right, tag = "equivalent", reason = "The earlier index-source root check and equality with record.source enforce the same containment.")]
            record.outputs.iter().any(|(path, hash)| {
                // #[gamma::skip(logical.and_remove_right, tag = "equivalent", reason = "The later all-inputs-and-outputs digest check independently verifies this same matching artifact hash.")]
                path == &artifact && *hash == expected
            }) && record.source.starts_with(root)
        ) && record.inputs.iter().any(|(path, _)| path == &record.source)
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
            && inventory(files, root, target) == Some(record.inventory))
        {
            return None;
        }
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
    #[cfg(test)]
    fn reusable(&self, files: &impl Files, record: &Record) -> bool {
        if record.context != self.context {
            return false;
        }
        self.snapshot(files, Vec::new(), Vec::new())
            .is_some_and(|current| same_snapshot(&current, record))
    }
}

fn same_snapshot(current: &Record, record: &Record) -> bool {
    current.inventory == record.inventory
        && current.inputs == record.inputs
        && current.outputs == record.outputs
        && current.searches == record.searches
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
    let before = unit.snapshot(files, Vec::new(), Vec::new());
    if let Some(record) = previous
        && record.context == unit.context
        && before.as_ref().is_some_and(|current| same_snapshot(current, record))
    {
        relay(&record.stdout, &record.stderr)?;
        return Ok(Some(0));
    }
    // Missing or changed dep-info cannot establish a closed capture until a later invocation has
    // observed the resulting graph both before and after compilation.
    let output = run()?;
    if !output.forwarded {
        relay(&output.stdout, &output.stderr)?;
    }
    if output.code == Some(0)
        && output.capturable
        && let Some(before) = before.as_ref()
        && let Some(record) = unit.snapshot(files, output.stdout, output.stderr)
        && stable_inputs(before, &record)
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
            let _published = publish_record(&directory, &path, record);
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
pub(in crate::exec) mod compiler_campaign_tests {
    use super::super::workspace::Workspace;
    use super::*;

    const SUBJECT_MANIFEST: &[u8] = b"[package]\nname='subject'\nversion='0.1.0'\n";
    const RUNTIME_MANIFEST: &[u8] = b"[package]\nname='cargo-gamma-rt'\nversion='0.1.0'\n[lib]\nname='gamma_rt'\n";
    #[cfg(not(miri))]
    const STANDALONE_RUNTIME_MANIFEST: &[u8] = b"[package]\nname='cargo-gamma-rt'\nversion='0.0.0'\n[lib]\nname='gamma_rt'\n[workspace]\n";
    const RUNTIME_SOURCE: &[u8] = b"pub fn selected() -> bool { false }";

    fn with_unit<T>(with_runtime: bool, operation: impl FnOnce(&Workspace, OwnedUnit, Record) -> T) -> T {
        let base = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap())
            .unwrap()
            .join("nov21-injected-compiler");
        let root = base.join("workspace");
        let target = root.join("target");
        let compiler = base.join("toolchain").join("bin").join("rustc.exe");
        let cargo = base.join("tools").join("cargo.exe");
        let source = root.join("src").join("lib.rs");
        let output = target.join("libsubject-unit.rmeta");
        let dep = target.join("subject-unit.d");
        let mut work = Workspace::adopt(root.clone(), target.clone());
        work.set_rustc_captures(Some(target.join("captures")));
        let runtime_root = work.compiler_runtime();
        let runtime_manifest = runtime_root.join("Cargo.toml");
        let runtime_source = runtime_root.join("src").join("lib.rs");
        let mut initial = vec![
            (root.join("Cargo.toml").into_std_path_buf(), SUBJECT_MANIFEST.to_vec()),
            (source.clone().into_std_path_buf(), b"pub fn value() -> u32 { 1 }".to_vec()),
            (compiler.clone().into_std_path_buf(), b"compiler identity".to_vec()),
            (cargo.clone().into_std_path_buf(), b"Cargo identity".to_vec()),
            (
                base.join("toolchain").join("lib").join("libstd.rlib").into_std_path_buf(),
                b"sysroot".to_vec(),
            ),
            (output.clone().into_std_path_buf(), b"rust\0metadata".to_vec()),
            (dep.into_std_path_buf(), format!("{output}: {source}\n").into_bytes()),
            (runtime_manifest.into_std_path_buf(), RUNTIME_MANIFEST.to_vec()),
            (runtime_source.into_std_path_buf(), RUNTIME_SOURCE.to_vec()),
        ];
        let environment = BTreeMap::from([
            ("CARGO".into(), cargo.into_std_path_buf().into_os_string()),
            ("USERPROFILE".into(), base.join("home").into_std_path_buf().into_os_string()),
        ]);
        let captures = snapshot_io::with(
            snapshot_io::Script {
                environment: environment.clone(),
                ..snapshot_io::Script::default()
            },
            || {
                let args = ["check".to_owned(), "--workspace".to_owned()];
                let mut command = work.cargo();
                let _command = command
                    .args(&args)
                    .env("CARGO_TERM_PROGRESS_WHEN", "always")
                    .env("CARGO_TERM_PROGRESS_WIDTH", PROGRESS_WIDTH.to_string());
                work.configure_compiler_replay(&mut command);
                let generation = command
                    .get_envs()
                    .find_map(|(name, value)| (name == REPLAY_VAR).then_some(value).flatten())
                    .unwrap()
                    .to_str()
                    .unwrap();
                work.rustc_captures().unwrap().join("replay").join(generation)
            },
        );
        initial.extend([
            (captures.join(".ready").into_std_path_buf(), Vec::new()),
            (
                captures.join("nested").join("interrupted.json").into_std_path_buf(),
                b"unpublished nested capture".to_vec(),
            ),
            (
                captures.join("interrupted.tmp").into_std_path_buf(),
                b"unpublished capture".to_vec(),
            ),
        ]);
        let mut script = snapshot_io::Script::from_files(initial);
        script.environment = environment;
        snapshot_io::with(script, || {
            let mut unit = OwnedUnit {
                context: [1; 32],
                invocation: RustcInvocation {
                    crate_name: "subject".into(),
                    crate_types: vec!["lib".into()],
                    test: false,
                    source: source.clone(),
                    out_dir: target.clone(),
                    extra_filename: "-unit".into(),
                    externs: Vec::new(),
                    opaque_extern: false,
                },
                cwd: root.clone(),
                root: root.clone(),
                target: target.clone(),
                compiler: compiler.clone(),
                searches: Vec::new(),
                captures: captures.clone(),
                runtime: None,
            };
            if with_runtime {
                capture_runtime(&work, &mut unit);
            }
            let record = unit.borrowed().snapshot(&NativeFiles, Vec::new(), Vec::new()).unwrap();
            publish_record(&captures, &captures.join("unit.json"), &record).unwrap();
            operation(&work, unit, record)
        })
    }

    fn capture_runtime(work: &Workspace, unit: &mut OwnedUnit) {
        let runtime_root = work.compiler_runtime();
        let runtime_source = runtime_root.join("src").join("lib.rs");
        let artifact = unit.target.join("libgamma_rt-runtime.rmeta");
        snapshot_io::write(runtime_root.join("Cargo.toml"), RUNTIME_MANIFEST).unwrap();
        snapshot_io::write(&runtime_source, RUNTIME_SOURCE).unwrap();
        snapshot_io::write(&artifact, b"rust\0runtime metadata").unwrap();
        snapshot_io::write(unit.target.join("gamma_rt-runtime.d"), format!("{artifact}: {runtime_source}\n")).unwrap();
        snapshot_io::write(
            unit.root.join("Cargo.toml"),
            format!(
                "[package]\nname='subject'\nversion='0.1.0'\n[dependencies]\ngamma_rt={{path={},package='cargo-gamma-rt'}}\n",
                serde_json::to_string(runtime_root.as_str()).unwrap(),
            ),
        )
        .unwrap();
        let mut command = Command::new(&unit.compiler);
        let _command = command.current_dir(&unit.root).args([
            OsString::from("--crate-name=gamma_rt"),
            OsString::from("--crate-type=lib"),
            OsString::from("--emit=dep-info,metadata"),
            OsString::from("--out-dir"),
            unit.target.clone().into(),
            OsString::from("-Cextra-filename=-runtime"),
            runtime_source.into(),
        ]);
        let environment = BTreeMap::from([
            (RUNTIME_VAR.into(), runtime_root.to_string()),
            (TARGET_VAR.into(), unit.target.to_string()),
            (COMMAND_VAR.into(), "parent Cargo command".into()),
            ("CARGO_MANIFEST_DIR".into(), runtime_root.to_string()),
        ]);
        let runtime = runtime::capture_with(&NativeFiles, &command, &environment, &unit.root).unwrap();
        snapshot_io::write(
            work.rustc_captures().unwrap().join("runtime-provenance"),
            serde_json::to_vec(&runtime).unwrap(),
        )
        .unwrap();
        unit.runtime = Some(runtime_root.to_owned());
        unit.invocation.externs.push(artifact);
    }

    pub(in crate::exec) fn with_metadata<T>(runtime: bool, operation: impl FnOnce(&Workspace, &[String], &str) -> T) -> T {
        with_unit(runtime, |work, unit, _| {
            let stdout = serde_json::json!({
                "reason":"compiler-artifact", "filenames":[unit.target.join("libsubject-unit.rmeta")]
            })
            .to_string();
            operation(work, &["check".to_owned(), "--workspace".to_owned()], &stdout)
        })
    }

    fn make_consumer(unit: &mut OwnedUnit) {
        unit.invocation.crate_name = "consumer".into();
        unit.context = [2; 32];
        let output = unit.target.join("libconsumer-unit.rmeta");
        snapshot_io::write(&output, b"rust\0consumer metadata").unwrap();
        snapshot_io::write(
            unit.target.join("consumer-unit.d"),
            format!("{output}: {}\n", unit.invocation.source),
        )
        .unwrap();
    }

    #[test]
    fn nov21_source_context_ignores_nested_json_and_unpublished_top_level_entries() {
        with_unit(false, |_, unit, _| {
            let stdout =
                serde_json::json!({"reason":"compiler-artifact","filenames":[unit.target.join("libsubject-unit.rmeta")]}).to_string();
            for path in ["nested/interrupted.json", "interrupted.tmp"] {
                snapshot_io::write(unit.captures.join(path), b"not a published record").unwrap();
            }
            let context = source_only_context_with(&NativeFiles, &unit.captures, &stdout, &unit.root, &unit.target, None).unwrap();
            assert_eq!(context.units, [[1; 32]]);
        });
    }

    #[cfg(not(miri))]
    #[test]
    fn nov21_a_standalone_runtime_check_cannot_seal_an_uncaptured_workspace() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let base = Utf8PathBuf::from_path_buf(directory.path().canonicalize().unwrap()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");
        let runtime_root = base.join("rt");
        let manifest = runtime_root.join("Cargo.toml");
        let capture_root = target.join("captures");
        let compiler = base.join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(crate::testing::helper_binary_path(), &compiler).unwrap();
        let output = target.join("libgamma_rt-runtime.rmeta");
        let source = runtime_root.join("src").join("lib.rs");
        let mut cargo = Command::new("cargo");
        let _cargo = cargo
            .current_dir(&root)
            .env("CARGO_TARGET_DIR", &target)
            .env("RUSTC", &compiler)
            .args([
                "check",
                "--keep-going",
                "--lib",
                "--message-format=json",
                "--workspace",
                "--manifest-path",
            ])
            .arg(&manifest);
        let mut environment = BTreeMap::from([
            (ROOT_VAR.to_owned(), root.to_string()),
            (TARGET_VAR.to_owned(), target.to_string()),
            (RUNTIME_VAR.to_owned(), runtime_root.to_string()),
            (CAPTURE_DIR_VAR.to_owned(), capture_root.to_string()),
            ("RUSTC".to_owned(), compiler.to_string()),
            ("CARGO_MANIFEST_DIR".to_owned(), runtime_root.to_string()),
        ]);
        let generation = blake3::Hash::from(
            command_fingerprint(
                &cargo,
                environment
                    .iter()
                    .map(|(name, value)| (OsString::from(name), OsString::from(value))),
            )
            .unwrap(),
        )
        .to_hex()
        .to_string();
        let _command = environment.insert(COMMAND_VAR.to_owned(), generation.clone());
        let _replay = environment.insert(REPLAY_VAR.to_owned(), generation.clone());
        let captures = capture_root.join("replay").join(generation);
        let initial = vec![
            (root.join("Cargo.toml").into_std_path_buf(), SUBJECT_MANIFEST.to_vec()),
            (root.join("src").join("lib.rs").into_std_path_buf(), b"pub fn value() {}".to_vec()),
            (manifest.clone().into_std_path_buf(), STANDALONE_RUNTIME_MANIFEST.to_vec()),
            (source.clone().into_std_path_buf(), RUNTIME_SOURCE.to_vec()),
            (compiler.clone().into_std_path_buf(), std::fs::read(&compiler).unwrap()),
            (output.clone().into_std_path_buf(), b"rust\0runtime metadata".to_vec()),
            (
                target.join("gamma_rt-runtime.d").into_std_path_buf(),
                format!("{}: {}\n", output.as_str().replace(' ', r"\ "), source.as_str().replace(' ', r"\ ")).into_bytes(),
            ),
            (captures.join(".ready").into_std_path_buf(), Vec::new()),
        ];
        for (path, bytes) in &initial {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            if path != compiler.as_std_path() {
                std::fs::write(path, bytes).unwrap();
            }
        }
        let mut script = snapshot_io::Script::from_files(initial);
        script.cwd = Some(runtime_root.clone().into_std_path_buf());
        script.environment = environment.iter().map(|(name, value)| (name.into(), value.into())).collect();
        let mut command = Command::new(&compiler);
        let _command = command.current_dir(&runtime_root).args([
            OsString::from("--crate-name=gamma_rt"),
            OsString::from("--crate-type=lib"),
            OsString::from("--emit=dep-info,metadata"),
            OsString::from("--out-dir"),
            target.clone().into(),
            OsString::from("-Cextra-filename=-runtime"),
            source.into(),
            OsString::from("--cap-lints=allow"),
        ]);
        snapshot_io::with(script, || {
            assert!(prepare_with(&command, &environment, &runtime_root).is_none());
            assert_eq!(compiler_status(&mut command).unwrap(), Some(0));
            let runtime = runtime::load(&NativeFiles, &capture_root).unwrap();
            assert!(runtime.valid(&NativeFiles, &runtime_root, Some(&compiler)));
            assert!(runtime.contains(&NativeFiles, &output));
            assert!(
                NativeFiles
                    .files(&captures, Utf8Path::new(""))
                    .unwrap()
                    .iter()
                    .all(|path| path.extension() != Some("json"))
            );
            let stdout = runtime_metadata_stdout(&runtime_root, &output);
            assert!(source_only_context(&captures, &stdout, &root, &target, &runtime_root).is_none());
        });
    }

    #[cfg(not(miri))]
    fn runtime_metadata_stdout(root: &Utf8Path, output: &Utf8Path) -> String {
        [
            serde_json::json!({
                "reason":"compiler-artifact", "package_id":"cargo-gamma-rt 0.0.0",
                "manifest_path":root.join("Cargo.toml"),
                "target":{"name":"gamma_rt","kind":["lib"],"src_path":root.join("src").join("lib.rs")},
                "profile":{"test":false}, "filenames":[output], "fresh":false, "executable":null,
            })
            .to_string(),
            serde_json::json!({"reason":"build-finished","success":true}).to_string(),
        ]
        .join("\n")
    }

    #[test]
    fn nov21_changed_runtime_artifacts_decline_dependency_closure_and_source_context() {
        with_unit(true, |work, unit, _| {
            let stdout =
                serde_json::json!({"reason":"compiler-artifact","filenames":[unit.target.join("libsubject-unit.rmeta")]}).to_string();
            assert!(
                source_only_context_with(
                    &NativeFiles,
                    &unit.captures,
                    &stdout,
                    &unit.root,
                    &unit.target,
                    Some(work.compiler_runtime())
                )
                .is_some()
            );
            snapshot_io::write(&unit.invocation.externs[0], b"rust\0changed runtime metadata").unwrap();
            assert!(unit.borrowed().snapshot(&NativeFiles, Vec::new(), Vec::new()).is_none());
            assert!(
                source_only_context_with(
                    &NativeFiles,
                    &unit.captures,
                    &stdout,
                    &unit.root,
                    &unit.target,
                    Some(work.compiler_runtime())
                )
                .is_none()
            );
        });
    }

    #[test]
    fn nov21_dependency_indexes_require_a_local_record_matching_the_indexed_source() {
        for case in ["invalid-json", "nested-record", "mismatched-source"] {
            with_unit(false, |_, mut unit, record| {
                let artifact = unit.target.join("libsubject-unit.rmeta");
                make_consumer(&mut unit);
                unit.invocation.externs.push(artifact.clone());
                let index_path = output_index(&unit.captures, &artifact);
                let mut index: OutputIndex = serde_json::from_slice(&snapshot_io::read(&index_path).unwrap()).unwrap();
                match case {
                    "invalid-json" => snapshot_io::write(&index_path, b"truncated").unwrap(),
                    "nested-record" => {
                        index.record = "nested/unit.json".into();
                        snapshot_io::write(unit.captures.join(&index.record), serde_json::to_vec(&record).unwrap()).unwrap();
                        snapshot_io::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
                    }
                    _ => {
                        index.source = unit.root.join("src").join("another.rs");
                        snapshot_io::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
                    }
                }
                assert!(unit.borrowed().snapshot(&NativeFiles, Vec::new(), Vec::new()).is_none(), "{case}");
            });
        }
    }

    #[test]
    fn nov21_equal_artifact_bytes_do_not_prove_that_a_record_produced_the_requested_path() {
        with_unit(false, |_, mut unit, mut record| {
            let artifact = unit.target.join("libsubject-unit.rmeta");
            make_consumer(&mut unit);
            let other = unit.target.join("libother-unit.rmeta");
            snapshot_io::write(&other, snapshot_io::read(&artifact).unwrap()).unwrap();
            for (path, _) in &mut record.outputs {
                if path == &artifact {
                    *path = other.clone();
                }
            }
            snapshot_io::write(unit.captures.join("unit.json"), serde_json::to_vec(&record).unwrap()).unwrap();
            unit.invocation.externs.push(artifact);
            assert!(unit.borrowed().snapshot(&NativeFiles, Vec::new(), Vec::new()).is_none());
        });
    }

    #[test]
    fn nov21_chunked_file_stamps_preserve_the_first_eight_bytes_and_the_full_length() {
        struct Chunked<'a> {
            bytes: &'a [u8],
            chunks: std::vec::IntoIter<usize>,
            ended: bool,
        }
        impl io::Read for Chunked<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                assert!(!self.ended, "a snapshot must stop reading after EOF");
                let count = self.chunks.next().unwrap_or(buf.len()).min(buf.len()).min(self.bytes.len());
                buf[..count].copy_from_slice(&self.bytes[..count]);
                self.bytes = &self.bytes[count..];
                self.ended = count == 0;
                Ok(count)
            }
        }
        for chunks in [vec![3, 7, 1], vec![1, 1, 1, 1, 1, 1, 1, 1, 3], vec![8, 3], vec![1]] {
            let bytes = b"!<arch>\nxyz";
            assert_eq!(
                stamp_reader(
                    Chunked {
                        bytes,
                        chunks: chunks.into_iter(),
                        ended: false
                    },
                    bytes.len() as u64
                )
                .unwrap(),
                FileStamp::of(bytes),
            );
        }
        let failure = stamp_reader(io::Cursor::new(b"x"), 0).unwrap_err();
        let limit = failure.get_ref().unwrap().downcast_ref::<SnapshotLimitExceeded>().unwrap();
        assert_eq!(limit.limit, 0);
    }

    #[test]
    fn nov21_snapshot_metadata_and_read_failures_keep_their_distinct_capture_refusals() {
        use snapshot_io::{FileData, Script};

        let path = Utf8PathBuf::from("snapshot-input");
        for (length, read_error, expected_kind) in [
            (Err(io::ErrorKind::PermissionDenied), None, io::ErrorKind::PermissionDenied),
            (Ok(MAX_FILE_BYTES + 1), Some(io::ErrorKind::PermissionDenied), io::ErrorKind::Other),
            (
                Ok(MAX_FILE_BYTES),
                Some(io::ErrorKind::PermissionDenied),
                io::ErrorKind::PermissionDenied,
            ),
        ] {
            let mut data = FileData::bytes(b"");
            data.length = length;
            data.read_error = read_error;
            let mut script = Script::default();
            script.files.insert(path.clone().into_std_path_buf(), Ok(data));
            snapshot_io::with(script, || {
                let failure = snapshot_stamp(&path).unwrap_err();
                assert_eq!(failure.kind(), expected_kind);
                if expected_kind == io::ErrorKind::Other {
                    let limit = failure.get_ref().unwrap().downcast_ref::<SnapshotLimitExceeded>().unwrap();
                    assert_eq!(limit.limit, MAX_FILE_BYTES);
                }
            });
        }
    }

    #[test]
    fn nov21_snapshot_capture_cap_admits_the_exact_boundary_and_refuses_growth() {
        use std::io::Read as _;

        let archive = |length| io::Cursor::new(b"!<arch>\n").chain(io::repeat(0).take(length - 8));
        let stamp = snapshot_stamp_with(archive(MAX_FILE_BYTES), MAX_FILE_BYTES).unwrap();
        assert_eq!(stamp.length, MAX_FILE_BYTES);
        assert_eq!(stamp.prefix, *b"!<arch>\n");
        let failure = snapshot_stamp_with(archive(MAX_FILE_BYTES + 1), MAX_FILE_BYTES).unwrap_err();
        let limit = failure.get_ref().unwrap().downcast_ref::<SnapshotLimitExceeded>().unwrap();
        assert_eq!(limit.limit, MAX_FILE_BYTES);
    }

    #[test]
    fn nov21_replay_publication_propagates_record_and_output_index_filesystem_failures() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
        let record = Record {
            context: [1; 32],
            inputs: Vec::new(),
            outputs: vec![(root.join("subject.rmeta"), [2; 32])],
            inventory: [3; 32],
            stdout: Vec::new(),
            stderr: Vec::new(),
            compiler: root.join("rustc.exe"),
            source: root.join("subject.rs"),
            externs: Vec::new(),
            searches: Vec::new(),
        };
        let blocker = root.join("not-a-directory");
        std::fs::write(&blocker, b"preserved").unwrap();
        assert!(publish_record(&root, &blocker.join("record.json"), &record).is_err());
        assert_eq!(std::fs::read(&blocker).unwrap(), b"preserved");
        std::fs::write(root.join("outputs"), b"index directory blocked").unwrap();
        let path = root.join("record.json");
        assert!(publish_record(&root, &path, &record).is_err());
        let published: Record = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(published.source, record.source);
        assert_eq!(published.outputs, record.outputs);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;
