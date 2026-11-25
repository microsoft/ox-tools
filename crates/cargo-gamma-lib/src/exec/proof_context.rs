// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Closed metadata-only proof identities. Dynamic Cargo graphs deliberately have no identity.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::ops::ControlFlow;
#[cfg(not(test))]
use std::{env, fs};

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use super::rustc_replay::snapshot_io::{self as fs, environment as env};
use super::rustc_replay::{PROGRESS_WIDTH, REPLAY_VAR, command_fingerprint, source_only_context};
use super::workspace::Workspace;
use crate::discover::Plan;

#[derive(Serialize, Deserialize)]
struct Context {
    command: [u8; 32],
    args: Vec<String>,
    immutable: Vec<(Utf8PathBuf, Option<[u8; 32]>)>,
    shape: Vec<Utf8PathBuf>,
    toolchain: Vec<(Utf8PathBuf, [u8; 32])>,
    units: Vec<[u8; 32]>,
    options: Vec<u8>,
    mutable: Vec<Utf8PathBuf>,
    runtime: Option<(Utf8PathBuf, Vec<Utf8PathBuf>)>,
    cargo: (Utf8PathBuf, [u8; 32]),
}

trait Snapshot {
    fn content(&self, path: &Utf8Path) -> std::io::Result<Option<[u8; 32]>>;
    fn text(&self, path: &Utf8Path) -> Option<String>;
    fn tree(&self, root: &Utf8Path, target: &Utf8Path) -> Option<Vec<Utf8PathBuf>>;
}

struct NativeSnapshot;

impl Snapshot for NativeSnapshot {
    fn content(&self, path: &Utf8Path) -> std::io::Result<Option<[u8; 32]>> {
        content(path)
    }

    fn text(&self, path: &Utf8Path) -> Option<String> {
        String::from_utf8(super::rustc_replay::read_snapshot(path).ok()?).ok()
    }

    fn tree(&self, root: &Utf8Path, target: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
        tree(root, target)
    }
}

fn content(path: &Utf8Path) -> std::io::Result<Option<[u8; 32]>> {
    match super::rustc_replay::digest_snapshot(path) {
        Ok(digest) => Ok(Some(digest)),
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(cause) => Err(cause),
    }
}

fn tree(root: &Utf8Path, target: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
    super::rustc_replay::snapshot_paths(root, target)
}

fn controls(root: &Utf8Path) -> Option<Vec<Utf8PathBuf>> {
    controls_with(root, |name| env::var_os(name), super::rustc_replay::unambiguous_path)
}

fn controls_with(
    root: &Utf8Path,
    environment: impl Fn(&str) -> Option<std::ffi::OsString>,
    unambiguous: impl Fn(&Utf8Path) -> bool,
) -> Option<Vec<Utf8PathBuf>> {
    let mut files = Vec::new();
    for ancestor in root.ancestors() {
        // #[gamma::skip(collection.reverse_array, tag = "equivalent", reason = "Control paths are sorted before use, so their insertion order is irrelevant.")]
        files.extend([
            ancestor.join(".cargo").join("config"),
            ancestor.join(".cargo").join("config.toml"),
            ancestor.join("rust-toolchain"),
            ancestor.join("rust-toolchain.toml"),
        ]);
    }
    let home = environment("USERPROFILE").or_else(|| environment("HOME"))?;
    let home = Utf8PathBuf::from_path_buf(home.into()).ok()?;
    let cargo = environment("CARGO_HOME").map_or_else(|| Some(home.join(".cargo")), |path| Utf8PathBuf::from_path_buf(path.into()).ok())?;
    let rustup =
        environment("RUSTUP_HOME").map_or_else(|| Some(home.join(".rustup")), |path| Utf8PathBuf::from_path_buf(path.into()).ok())?;
    if !home.is_absolute() || !cargo.is_absolute() || !rustup.is_absolute() {
        return None;
    }
    // #[gamma::skip(collection.reverse_array, tag = "equivalent", reason = "Control paths are sorted before use, so their insertion order is irrelevant.")]
    files.extend([cargo.join("config"), cargo.join("config.toml"), rustup.join("settings.toml")]);
    files.sort();
    // #[gamma::skip(iter.remove_dedup, tag = "equivalent", reason = "The sole caller immutable_snapshot deduplicates the complete path vector again.")]
    files.dedup();
    if files.iter().any(|path| !unambiguous(path)) {
        return None;
    }
    Some(files)
}

fn source_only_manifest(value: &toml::Value, runtime: Option<&Utf8Path>) -> bool {
    match value {
        toml::Value::Table(table) => table.iter().all(|(key, value)| {
            if key.ends_with("dependencies") {
                value.as_table().is_some_and(|dependencies| {
                    dependencies.iter().all(|(name, value)| {
                        if !matches!(name.as_str(), "gamma_rt" | "cargo-gamma-rt") {
                            return false;
                        }
                        let Some(entry) = value.as_table() else { return false };
                        entry.get("workspace").and_then(toml::Value::as_bool) == Some(true)
                            || runtime.is_some_and(|runtime| {
                                entry
                                    .get("path")
                                    .and_then(toml::Value::as_str)
                                    .is_some_and(|path| Utf8Path::new(path) == runtime)
                                    && entry
                                        .get("package")
                                        .and_then(toml::Value::as_str)
                                        .is_none_or(|name| name == "cargo-gamma-rt")
                            })
                    })
                })
            } else if key == "build" || key == "proc-macro" {
                value.as_bool() == Some(false)
            } else {
                source_only_manifest(value, runtime)
            }
        }),
        toml::Value::Array(values) => values.iter().all(|value| source_only_manifest(value, runtime)),
        _ => true,
    }
}

/// Macro token trees may hide filesystem access behind a `macro_rules` expansion. Source-only
/// identities therefore decline *any* occurrence of filesystem-expansion identifiers.
fn source_only_tokens(tokens: proc_macro2::TokenStream) -> bool {
    tokens.into_iter().all(|token| match token {
        proc_macro2::TokenTree::Group(group) => source_only_tokens(group.stream()),
        // #[gamma::skip(collection.reverse_array, tag = "equivalent", reason = "The identifier list is tested only by a pure membership predicate.")]
        proc_macro2::TokenTree::Ident(ident) => ![
            "include",
            "include_str",
            "include_bytes",
            "path",
            "global_asm",
            "asm",
            "env",
            "option_env",
        ]
        .iter()
        .any(|name| ident == *name),
        _ => true,
    })
}

fn closed_source(path: &Utf8Path, snapshot: &impl Snapshot) -> Option<()> {
    let tokens: proc_macro2::TokenStream = snapshot.text(path)?.parse().ok()?;
    (super::rustc_replay::source_tokens_closed(tokens.clone()) && source_only_tokens(tokens)).then_some(())
}

fn closed_sources(paths: &[Utf8PathBuf], snapshot: &impl Snapshot, runtime: Option<&Utf8Path>) -> Option<()> {
    let mut manifests = 0;
    for path in paths {
        if path.file_name() == Some("build.rs") {
            return None;
        }
        if path.file_name() == Some("Cargo.toml") {
            let manifest = toml::from_str::<toml::Value>(&snapshot.text(path)?).ok()?;
            if !source_only_manifest(&manifest, runtime) {
                return None;
            }
            // #[gamma::skip(assign.add_to_sub, literal.int_increment, tag = "equivalent", reason = "Only manifest presence is observed; decrementing or adding two stays nonzero within the bounded source tree.")]
            manifests += 1;
        }
        if path.extension() == Some("rs") {
            closed_source(path, snapshot)?;
        }
    }
    (manifests != 0).then_some(())
}

fn immutable_snapshot(work: &Workspace, plan: &Plan, shape: &[Utf8PathBuf]) -> Option<Vec<(Utf8PathBuf, Option<[u8; 32]>)>> {
    immutable_paths(work, plan, shape)?
        .into_iter()
        .map(|path| Some((path.clone(), content(&path).ok()?)))
        .collect()
}

fn immutable_paths(work: &Workspace, plan: &Plan, shape: &[Utf8PathBuf]) -> Option<Vec<Utf8PathBuf>> {
    let mutable: BTreeSet<_> = plan.files.iter().map(|file| work.root.join(&file.path)).collect();
    let mut paths: Vec<_> = shape.iter().filter(|path| !mutable.contains(*path)).cloned().collect();
    paths.extend(controls(&work.root)?);
    // #[gamma::skip(iter.remove_sort, tag = "equivalent", reason = "Both input vectors are deterministically ordered, so sealing and revalidation preserve the same path order without this canonicalization.")]
    paths.sort();
    // #[gamma::skip(iter.remove_dedup, tag = "equivalent", reason = "Retaining the same duplicate immutable paths in sealing and revalidation changes only opaque identity encoding.")]
    paths.dedup();
    Some(paths)
}

fn mutable_sources(work: &Workspace, plan: &Plan) -> Vec<Utf8PathBuf> {
    let mut paths: Vec<_> = plan.files.iter().map(|file| work.root.join(&file.path)).collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Seals only a complete successful `cargo check` whose artifact set is present in replay captures.
/// A fresh Cargo unit without a complete capture makes the entire identity unavailable.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn seal(work: &Workspace, plan: &Plan, args: &[String], stdout: &str) {
    #[cfg(test)]
    if tests::observe(Operation::Seal(args, stdout)).is_break() {
        return;
    }
    let Some(captures) = work.rustc_captures() else { return };
    let path = captures.join("proof-context");
    let _removed = fs::remove_file(&path);
    let Some(context) = seal_context(work, plan, args, stdout) else {
        return;
    };
    if let Ok(bytes) = serde_json::to_vec(&context) {
        let _published = fs::write(path, bytes);
    }
}

pub(super) fn invalidate(work: &Workspace) {
    #[cfg(test)]
    if tests::observe(Operation::Invalidate).is_break() {
        return;
    }
    if let Some(captures) = work.rustc_captures() {
        let _removed = fs::remove_file(captures.join("proof-context"));
    }
}

fn seal_context(work: &Workspace, plan: &Plan, args: &[String], stdout: &str) -> Option<Context> {
    if args.first().map(String::as_str) != Some("check") {
        return None;
    }
    let _outputs = super::rustc_replay::metadata_outputs(stdout)?;
    let shape = tree(&work.root, &work.target)?;
    closed_sources(&shape, &NativeSnapshot, Some(work.compiler_runtime()))?;
    let cargo_command = work.cargo();
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Workspace::cargo constructs its program from cargo_binary's UTF-8 String.")]
    let cargo = Utf8PathBuf::from(cargo_command.get_program().to_str()?);
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "A successful Cargo executable has a filename; a filesystem root cannot execute the captured cargo check.")]
    if !cargo.is_absolute() || cargo.file_stem()? != "cargo" || !super::rustc_replay::unambiguous_path(&cargo) {
        return None;
    }
    let fingerprint = command_fingerprint(&cargo_command, env::vars_os())?;
    let mut command = cargo_command;
    let _command = command
        .args(args)
        .env("CARGO_TERM_PROGRESS_WHEN", "always")
        .env("CARGO_TERM_PROGRESS_WIDTH", PROGRESS_WIDTH.to_string());
    work.configure_compiler_replay(&mut command);
    let generation = command
        .get_envs()
        .find_map(|(name, value)| (name == REPLAY_VAR).then_some(value).flatten())?;
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "configure_compiler_replay writes REPLAY_VAR from an ASCII hexadecimal digest.")]
    let generation = generation.to_str()?;
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "seal already obtained the same owned capture directory; this build sequence does not change capture readiness.")]
    let directory = work.rustc_captures()?.join("replay").join(generation);
    let source = source_only_context(&directory, stdout, &work.root, &work.target, work.compiler_runtime())?;
    if source.runtime.is_none() {
        closed_sources(&shape, &NativeSnapshot, None)?;
    }
    let cargo_hash = source
        .toolchain
        .iter()
        .find(|(path, _)| path == &cargo)
        .map(|(_, hash)| *hash)
        .or_else(|| content(&cargo).ok().flatten())?;
    let runtime = match &source.runtime {
        Some(root) => Some((root.clone(), tree(root, &work.target)?)),
        None => None,
    };
    Some(Context {
        command: fingerprint,
        args: args.to_vec(),
        immutable: immutable_snapshot(work, plan, &shape)?,
        shape,
        toolchain: source.toolchain,
        units: source.units,
        options: serde_json::to_vec(&work.cargo).ok()?, // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Cargo options contain only strings, booleans, and vectors with derived JSON serialization.")]
        mutable: mutable_sources(work, plan),
        runtime,
        cargo: (cargo, cargo_hash),
    })
}

/// Returns an exact identity only while the sealed static source-only context remains closed.
pub(super) fn identity(work: &Workspace, plan: &Plan) -> Option<[u8; 32]> {
    #[cfg(test)]
    if let ControlFlow::Break(identity) = tests::observe(Operation::Identity(plan)) {
        return identity;
    }
    let captures = work.rustc_captures()?;
    let context: Context = serde_json::from_slice(&fs::read(captures.join("proof-context")).ok()?).ok()?;
    if context.mutable != mutable_sources(work, plan) {
        return None;
    }
    if context.runtime.as_ref().is_some_and(|(root, _)| root != work.compiler_runtime()) {
        return None;
    }
    let shape = tree(&work.root, &work.target)?;
    let immutable = immutable_paths(work, plan, &shape)?;
    if !immutable.iter().eq(context.immutable.iter().map(|(path, _)| path)) {
        return None;
    }
    context_identity(
        &context,
        command_fingerprint(&work.cargo(), env::vars_os())?,
        &serde_json::to_vec(&work.cargo).ok()?, // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Cargo options contain only strings, booleans, and vectors with derived JSON serialization.")]
        &shape,
        &context.immutable,
        &work.target,
        &NativeSnapshot,
    )
}

fn context_identity(
    context: &Context,
    command: [u8; 32],
    options: &[u8],
    shape: &[Utf8PathBuf],
    immutable: &[(Utf8PathBuf, Option<[u8; 32]>)],
    target: &Utf8Path,
    snapshot: &impl Snapshot,
) -> Option<[u8; 32]> {
    if context.command != command {
        return None;
    }
    if context.options != options {
        return None;
    }
    if shape != context.shape || immutable != context.immutable {
        return None;
    }
    let mut stamps = BTreeMap::new();
    for (path, expected) in immutable {
        let actual = snapshot.content(path).ok()?;
        if actual != *expected {
            return None;
        }
        let _previous = stamps.insert(path.as_path(), actual);
    }
    let cargo_hash = match stamps.get(context.cargo.0.as_path()) {
        Some(stamp) => *stamp,
        None => snapshot.content(&context.cargo.0).ok()?,
    }?;
    if cargo_hash != context.cargo.1 {
        return None;
    }
    let _previous = stamps.insert(context.cargo.0.as_path(), Some(cargo_hash));
    for path in shape {
        // #[gamma::skip(logical.and_remove_right, tag = "equivalent", reason = "The mutable inventory contains only discovered Rust source paths, already ending in rs.")]
        if context.mutable.binary_search(path).is_ok() && path.extension() == Some("rs") {
            closed_source(path, snapshot)?;
        }
    }
    if let Some((root, expected)) = &context.runtime
        && snapshot.tree(root, target)? != *expected
    {
        return None;
    }
    let mut hash = blake3::Hasher::new();
    // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The serialized arguments, unit digests, and immutable path stamps contain only strings, paths, byte arrays, vectors, and Options.")]
    let _hash = hash
        .update(&context.command)
        .update(&serde_json::to_vec(&context.args).ok()?)
        .update(&serde_json::to_vec(&context.units).ok()?)
        .update(&serde_json::to_vec(&context.immutable).ok()?)
        .update(&context.options)
        .update(&context.cargo.1);
    for path in shape {
        let _hash = hash.update(path.as_str().as_bytes());
    }
    for (path, expected) in &context.toolchain {
        let actual = if let Some(stamp) = stamps.get(path.as_path()) {
            *stamp
        } else {
            let actual = snapshot.content(path).ok()?;
            let _previous = stamps.insert(path.as_path(), actual);
            actual
        }?;
        if actual != *expected {
            return None;
        }
        let _hash = hash.update(path.as_str().as_bytes()).update(&actual);
    }
    let expected: BTreeSet<_> = context.toolchain.iter().map(|(path, _)| path.clone()).collect();
    for (compiler, _) in context.toolchain.iter().filter(|(path, _)| path.file_stem() == Some("rustc")) {
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Every emitted toolchain path is absolute; a selected rustc filename cannot be a parentless filesystem root.")]
        let bin = compiler.parent()?;
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "Selected inputs are descendants of owned runtime roots or compiler bin/lib directories, so their parent has a directory name.")]
        if bin.file_name()? != "bin" {
            return None;
        }
        // #[gamma::skip(try.propagate_to_unwrap, tag = "equivalent", reason = "The preceding guard established the normal directory name bin, which necessarily has a parent.")]
        let toolchain = bin.parent()?;
        let mut paths = snapshot.tree(bin, target)?;
        paths.extend(snapshot.tree(&toolchain.join("lib"), target)?);
        if paths.into_iter().any(|path| !expected.contains(&path)) {
            return None;
        }
    }
    Some(*hash.finalize().as_bytes())
}

#[cfg(test)]
pub(super) use tests::{Operation, with_operations};

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod compiler_campaign_tests {
    use super::super::rustc_replay::compiler_campaign_tests::with_metadata;
    use super::super::rustc_replay::snapshot_io::{self, Script};
    use super::*;

    fn plan(work: &Workspace) -> Plan {
        Plan {
            root: work.root.clone(),
            ..crate::fixtures::plan(Vec::new())
        }
    }

    #[test]
    fn nov21_sealing_requires_metadata_and_a_still_readable_selected_cargo_binary() {
        with_metadata(false, |work, args, stdout| {
            assert!(seal_context(work, &plan(work), args, stdout).is_some());
            assert!(seal_context(work, &plan(work), args, "invalid JSON").is_none());
            let cargo = Utf8PathBuf::from(work.cargo().get_program().to_str().unwrap());
            snapshot_io::remove_file(cargo).unwrap();
            assert!(seal_context(work, &plan(work), args, stdout).is_none());
        });
    }

    #[test]
    fn nov21_runtime_path_dependencies_require_provenance_but_do_not_require_dependency_free_revalidation() {
        with_metadata(true, |work, args, stdout| {
            let context = seal_context(work, &plan(work), args, stdout).unwrap();
            assert_eq!(
                context.runtime.as_ref().map(|(root, _)| root.as_path()),
                Some(work.compiler_runtime())
            );
        });
        with_metadata(false, |work, args, stdout| {
            snapshot_io::write(
                work.compiler_runtime().join("Cargo.toml"),
                b"[package]\nname='cargo-gamma-rt'\nversion='0.1.0'\n[lib]\nname='gamma_rt'\n",
            )
            .unwrap();
            snapshot_io::write(
                work.compiler_runtime().join("src").join("lib.rs"),
                b"pub fn selected() -> bool { false }",
            )
            .unwrap();
            snapshot_io::write(work.root.join("Cargo.toml"), format!(
                "[package]\nname='subject'\nversion='0.1.0'\n[dependencies]\ngamma_rt={{path={},package='cargo-gamma-rt',optional=true}}\n",
                serde_json::to_string(work.compiler_runtime().as_str()).unwrap(),
            )).unwrap();
            let captures = work.rustc_captures().unwrap();
            let record_path = super::super::rustc_replay::snapshot_paths(&captures, Utf8Path::new(""))
                .unwrap()
                .into_iter()
                .find(|path| path.file_name() == Some("unit.json"))
                .unwrap();
            let mut record: serde_json::Value = serde_json::from_slice(&snapshot_io::read(&record_path).unwrap()).unwrap();
            record["inventory"] = serde_json::to_value(snapshot_io::inventory(&work.root, &work.target).unwrap()).unwrap();
            snapshot_io::write(record_path, serde_json::to_vec(&record).unwrap()).unwrap();
            assert!(seal_context(work, &plan(work), args, stdout).is_none());
        });
    }

    #[test]
    fn nov21_identity_rejects_a_truncated_immutable_inventory_and_newly_ambiguous_controls() {
        with_metadata(false, |work, args, stdout| {
            let plan = plan(work);
            seal(work, &plan, args, stdout);
            assert!(identity(work, &plan).is_some());
            let context_path = work.rustc_captures().unwrap().join("proof-context");
            let bytes = snapshot_io::read(&context_path).unwrap();
            let mut malformed: Context = serde_json::from_slice(&bytes).unwrap();
            malformed.immutable.retain(|(path, _)| path.file_name() != Some("Cargo.toml"));
            snapshot_io::write(&context_path, serde_json::to_vec(&malformed).unwrap()).unwrap();
            assert_eq!(identity(work, &plan), None);
            snapshot_io::write(&context_path, bytes).unwrap();

            let paths = super::super::rustc_replay::snapshot_paths(work.root.parent().unwrap(), Utf8Path::new("")).unwrap();
            let mut script = Script::from_files(paths.into_iter().map(|path| {
                let bytes = snapshot_io::read(&path).unwrap();
                (path.into_std_path_buf(), bytes)
            }));
            script.environment = env::vars_os().collect();
            let home = Utf8PathBuf::from_path_buf(env::var_os("USERPROFILE").unwrap().into()).unwrap();
            script
                .symlinks
                .insert(home.join(".cargo").join("config.toml").into_std_path_buf(), Ok(true));
            snapshot_io::with(script, || assert_eq!(identity(work, &plan), None));
        });
    }
}
