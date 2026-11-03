// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Supported in-process updates with caller-supplied repository I/O.

use std::ffi::OsString;
use std::path::Path;

use ohno::AppError;

use crate::{Backend, Catalog};

/// One immediate repository directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RepositoryEntry {
    /// A single native path component.
    pub name: OsString,
    /// Whether this entry resolves to a directory.
    pub is_directory: bool,
}

impl RepositoryEntry {
    /// Describe an immediate file or directory entry.
    #[must_use]
    pub fn new(name: impl Into<OsString>, is_directory: bool) -> Self {
        Self {
            name: name.into(),
            is_directory,
        }
    }
}

/// External repository operations used by [`update`].
///
/// Paths are relative to the repository root. File contents must be UTF-8.
/// Return `Ok(None)` for missing files and directories. Other failures must be
/// returned, not hidden. Directory-entry names must be single native components,
/// not paths.
///
/// A filesystem implementation must enforce repository containment (including
/// symlinks) before writing or deleting, and replace files atomically. The CLI
/// supplies such an implementation; an in-memory implementation needs no host I/O.
pub trait UpdateIo {
    /// Read a file, returning `None` only when it does not exist.
    ///
    /// # Errors
    ///
    /// Returns read failures, including invalid UTF-8.
    fn read_file(&self, path: &Path) -> Result<Option<String>, AppError>;

    /// List immediate entry names, including directories, in arbitrary order.
    ///
    /// The empty path denotes the repository root. Return `None` when no
    /// directory exists at this path, including when it names a regular file.
    /// Names must be single native path components.
    ///
    /// # Errors
    ///
    /// Returns directory access or enumeration failures.
    fn read_directory(&self, path: &Path) -> Result<Option<Vec<RepositoryEntry>>, AppError>;

    /// Replace a file, creating missing parent directories.
    ///
    /// # Errors
    ///
    /// Returns containment, directory creation, or publication failures.
    fn write_file(&mut self, path: &Path, content: &str) -> Result<(), AppError>;

    /// Remove a file.
    ///
    /// # Errors
    ///
    /// Returns deletion failures. `NotFound` is tolerated by the engine.
    fn remove_file(&mut self, path: &Path) -> std::io::Result<()>;
}

/// Explicit options for an in-process update.
///
/// No arguments, environment variables, Git remotes, or current directory are
/// consulted. An empty backend set means local-only generation.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct UpdateOptions {
    /// Cloud backends to generate. Duplicates are ignored.
    pub backends: Vec<Backend>,
    /// Plan without calling either mutating I/O operation.
    pub dry_run: bool,
    /// Permit switching ownership from a different anvil-family tool.
    pub force: bool,
}

/// An update result without exposing engine implementation types.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct UpdateResult {
    /// The resolved, sorted and deduplicated backend set.
    pub backends: Vec<Backend>,
    /// Whether application completed (false for dry runs).
    pub applied: bool,
    /// Whether the inspected input needs file or lock metadata updates.
    pub has_changes: bool,
    /// True when the inspected input has no drift and no refusals.
    ///
    /// The CLI `--dry-run` would exit with status 0 for this input.
    pub in_sync: bool,
    /// Stable human-readable plan summary.
    pub summary: String,
    /// Artifacts the engine refused to manage safely.
    pub refusals: Vec<String>,
    /// Informational planning diagnostics.
    pub notes: Vec<String>,
}

/// Update the Cargo repository represented by `io` using the real engine.
///
/// `Cargo.toml`, workspace members, and `.anvil.lock` are parsed from `io`.
/// All artifact inspection and application also use `io`. This function does
/// not initialize tracing, print output, launch processes, or access the host
/// filesystem, environment, arguments, or working directory.
///
/// Safety refusals are reported in the result, as with the CLI; safe independent
/// changes may still be applied. Application is not a repository-wide transaction:
/// an I/O failure may leave earlier writes in place, but the lock is published
/// only after all artifact operations succeed.
///
/// # Errors
///
/// Returns invalid workspace/lock data, ownership conflicts, ambiguous path
/// casing, and I/O failures. Dry runs perform all reads but no writes or deletes.
pub fn update(catalog: &Catalog, options: &UpdateOptions, io: &mut dyn UpdateIo) -> Result<UpdateResult, AppError> {
    let outcome = crate::run::update_with_io(catalog, options, io)?;
    Ok(UpdateResult {
        backends: outcome.backends,
        applied: outcome.applied,
        has_changes: outcome.plan.has_changes(),
        in_sync: outcome.plan.dry_run_exit_code() == 0,
        summary: outcome.plan.summary(Some(&outcome.previous_manifest)),
        refusals: outcome.plan.refusals().to_vec(),
        notes: outcome.plan.notes().to_vec(),
    })
}
