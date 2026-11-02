// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The workspace inputs a run had before it copied or executed anything.

use core::sync::atomic::{AtomicBool, Ordering};
use std::fs;
use std::sync::Mutex;

use camino::{Utf8Path, Utf8PathBuf};
use ignore::{WalkBuilder, WalkState};
use serde::{Deserialize, Serialize};

use super::Plan;
use super::record::digest;

/// A file in the workspace input snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct SnapshotFile {
    pub(super) path: Utf8PathBuf,
    pub(super) digest: String,
    pub(super) size: u64,

    /// The file's modification time, in nanoseconds since the epoch, where the platform reports one.
    ///
    /// Content alone cannot answer the question the snapshot is asked. A file edited during the run
    /// and restored to its original bytes before the run ends is byte-identical to what was
    /// captured, so a comparison of digests says the workspace never moved — while the outcomes
    /// were produced against the intermediate bytes. The modification time is an identity an
    /// edit-and-revert does not forge: restoring the content writes the file again, and writing it
    /// moves the time forward.
    ///
    /// Optional because not every platform and filesystem reports one, and a snapshot that refused
    /// to be taken there would disable the cache rather than protect it. Where it is absent on both
    /// sides the comparison degrades to the content check it always was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) modified: Option<u64>,
}

/// One local Cargo dependency whose source lies outside the workspace.
///
/// Paths below this root use the same relative spelling as workspace files, while the root itself
/// remains in the record so a later capture can re-read the same external input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExternalInput {
    root: Utf8PathBuf,
    files: Vec<SnapshotFile>,
}

/// The inputs a run had before it copied or executed the workspace.
///
/// Cargo input discovery is intentionally conservative: every regular workspace file is an input
/// unless it is generated under `target`, under gamma's scratch base, or under `.git`. This covers
/// source and test modules, manifests, lockfiles, build scripts and workspace cargo configuration
/// without hashing artifacts the build produced itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceSnapshot {
    /// A failed walk, read or non-UTF-8 path makes the snapshot unfit for carrying outcomes.
    #[serde(default)]
    complete: bool,

    /// Workspace-relative scratch paths that were excluded when this snapshot was taken.
    #[serde(default)]
    excluded: Vec<Utf8PathBuf>,

    /// Every included regular file, sorted by path.
    #[serde(default)]
    pub(super) files: Vec<SnapshotFile>,

    /// Local path dependencies outside the workspace.
    #[serde(default)]
    external: Vec<ExternalInput>,
}

impl WorkspaceSnapshot {
    /// Captures a complete conservative workspace input set.
    #[cfg(any(test, feature = "internals"))]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub(super) fn capture(root: &Utf8Path, excluded: &[Utf8PathBuf]) -> Self {
        Self::capture_with_external(root, excluded, &[], false)
    }

    /// Captures the workspace and every local path dependency Cargo can compile against.
    // #[gamma::skip(all, reason = "snapshot capture combines physical filesystem identity, external roots, and parallel traversal; exact cache eligibility is covered by filesystem integration tests and several branches are platform-dependent")]
    pub(super) fn capture_with_external(
        root: &Utf8Path,
        excluded: &[Utf8PathBuf],
        external_roots: &[Utf8PathBuf],
        untracked_build_script_inputs: bool,
    ) -> Self {
        let mut snapshot = Self {
            // Cargo build scripts can read arbitrary filesystem paths. Unless their inputs have
            // been reported and captured, retaining outcomes would make an external file change
            // invisible; no cache is safer than treating the script source as its whole input.
            complete: !untracked_build_script_inputs,
            excluded: exclusions(root, excluded),
            files: Vec::new(),
            external: Vec::new(),
        };
        snapshot.files = capture_tree(root, &snapshot.excluded, &mut snapshot.complete);

        let workspace = match crate::paths::physical(root) {
            Ok(path) => path,
            Err(_unresolved) => {
                snapshot.complete = false;
                return snapshot;
            }
        };
        let mut roots = Vec::new();

        for root in external_roots {
            match crate::paths::physical(root) {
                Ok(root) if !root.starts_with(&workspace) => roots.push(root),
                Ok(_inside_workspace) => {}
                Err(_unresolved) => snapshot.complete = false,
            }
        }

        roots.sort();
        roots.dedup();

        for root in roots {
            let mut excluded = exclusions(&root, &[]);

            // A path dependency may be an ancestor of the workspace. Its snapshot must not walk
            // back down into the workspace (and its scratch artifacts) a second time; those bytes
            // are already captured by the workspace half under its own exclusions.
            if let Ok(relative) = workspace.strip_prefix(&root)
                && !relative.as_str().is_empty()
            {
                excluded.push(relative.to_path_buf());
                excluded.sort();
                excluded.dedup();
            }

            let files = capture_tree(&root, &excluded, &mut snapshot.complete);

            snapshot.external.push(ExternalInput { root, files });
        }

        snapshot
    }

    /// Whether the workspace remains exactly as it was before the run.
    ///
    /// Equality covers each file's path, content, size and modification time, so an edit that is
    /// reverted before the run ends is still refused: reverting rewrites the file, and rewriting it
    /// moves its modification time past the one recorded. Content equality alone would have called
    /// such a workspace unchanged and carried forward outcomes that were produced against the
    /// intermediate bytes.
    ///
    /// Two gaps remain, both accepted. A filesystem whose modification times have coarse
    /// granularity — one second is the classic case — hides an edit-and-revert cycle that begins
    /// and ends inside a single tick. And a platform that reports no modification time at all
    /// leaves the comparison as the content check it always was.
    pub(super) fn matches_current(&self, root: &Utf8Path) -> bool {
        self.matches_snapshot(|| self.recapture(root))
    }

    fn matches_snapshot(&self, current: impl FnOnce() -> Self) -> bool {
        self.complete && *self == current()
    }

    pub(super) fn recapture(&self, root: &Utf8Path) -> Self {
        let external_roots: Vec<Utf8PathBuf> = self.external.iter().map(|input| input.root.clone()).collect();

        Self::capture_with_external(root, &self.excluded, &external_roots, false)
    }

    pub(super) const fn is_complete(&self) -> bool {
        self.complete
    }

    pub(super) fn matches_compilation_inputs(&self, current: &Self, roots: &[Utf8PathBuf]) -> bool {
        self.complete
            && current.complete
            && self.external == current.external
            && self
                .files
                .iter()
                .filter(|file| Self::is_compilation_input(&file.path, roots))
                .eq(current.files.iter().filter(|file| Self::is_compilation_input(&file.path, roots)))
    }

    /// The pre-execution metadata for one workspace-relative file.
    pub(super) fn file(&self, path: &Utf8Path) -> Option<&SnapshotFile> {
        let index = self.files.binary_search_by(|file| file.path.as_path().cmp(path)).ok()?;

        self.files.get(index)
    }

    /// Whether every source generation discovered for a completed plan belongs to this snapshot.
    pub(super) fn contains_plan_sources(&self, plan: &Plan) -> bool {
        let retained: crate::HashMap<&Utf8Path, &str> = plan
            .files
            .iter()
            .filter_map(|file| file.source.as_deref().map(|source| (file.path.as_path(), source)))
            .collect();

        plan.digests.iter().all(|(path, expected)| {
            let Some(file) = self.file(path) else {
                return false;
            };

            Self::source_generation_matches(&file.digest, expected, retained.get(path.as_path()).copied())
        })
    }

    fn is_compilation_input(path: &Utf8Path, roots: &[Utf8PathBuf]) -> bool {
        matches!(
            path.as_str(),
            "Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml"
        ) || path.starts_with(".cargo")
            || roots.iter().any(|root|
            // #[gamma::skip(logical.or_remove_left, reason = "Utf8Path::starts_with compares path-component prefixes. The empty path has no components and is a prefix of every path, including empty and absolute paths. Therefore root.is_empty() implies path.starts_with(root), making the removed left operand logically redundant for every Utf8Path input.", tag = "equivalent")]
            root.as_str().is_empty() || path.starts_with(root))
    }

    fn source_generation_matches(snapshot: &str, normalized: &str, retained: Option<&str>) -> bool {
        snapshot == normalized || retained.is_some_and(|source| snapshot == digest(source.as_bytes()))
    }

    /// Rust files known before the run, for the declaring-file index.
    #[cfg(any(test, feature = "internals"))]
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub(super) fn rust_files(&self, root: &Utf8Path) -> Vec<Utf8PathBuf> {
        self.files
            .iter()
            .filter(|file| file.path.extension() == Some("rs"))
            .map(|file| root.join(&file.path))
            .collect()
    }
}

/// Captures every regular file below one independently tracked source root.
///
/// The walk fans out over `ignore`'s worker pool with every filter it would otherwise apply
/// turned off, so the set of files visited is exactly the one a single-threaded walk would visit.
/// Each worker computes its own digests and reads its own metadata without holding any lock; the
/// only shared state is the completion flag, set with a relaxed store because the parallel walk
/// joins every worker thread before this function reads it back, and the results list, locked only
/// long enough to push one already-computed [`SnapshotFile`].
// #[gamma::skip(all, reason = "parallel filesystem walking, symlink handling, and completion propagation are covered by platform-specific snapshot tests; isolated mutations are scheduling or platform dependent")]
fn capture_tree(root: &Utf8Path, excluded: &[Utf8PathBuf], complete: &mut bool) -> Vec<SnapshotFile> {
    let boundary = fs::canonicalize(root.as_std_path())
        .ok()
        .and_then(|path| Utf8PathBuf::from_path_buf(path).ok());
    if boundary.is_none() {
        *complete = false;
    }

    let complete_flag = AtomicBool::new(*complete);
    let files: Mutex<Vec<SnapshotFile>> = Mutex::new(Vec::new());
    let excluded = excluded.to_vec();
    let root_owned = root.to_owned();

    let mut builder = WalkBuilder::new(root.as_std_path());

    let _builder = builder
        // Every regular file below the root is a candidate input; none of `ignore`'s conventions
        // for hidden files, parent-directory ignore files or `.git`-relative rules apply here.
        .hidden(false)
        .parents(false)
        .require_git(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .ignore(false)
        // A link is recorded by its own spelling below, never followed.
        .follow_links(false);

    builder.build_parallel().run(|| {
        let root = root_owned.clone();
        let excluded = excluded.clone();
        let boundary = boundary.clone();
        let complete_flag = &complete_flag;
        let files = &files;

        Box::new(move |entry| {
            let Ok(entry) = entry else {
                complete_flag.store(false, Ordering::Relaxed);
                return WalkState::Continue;
            };
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.path().to_path_buf()) else {
                complete_flag.store(false, Ordering::Relaxed);
                return WalkState::Continue;
            };
            let relative = path.strip_prefix(&root).unwrap_or(&path);
            let is_dir = entry.file_type().is_some_and(|file_type| file_type.is_dir());

            if excluded.iter().any(|excluded| relative.starts_with(excluded)) {
                return if is_dir { WalkState::Skip } else { WalkState::Continue };
            }

            if is_dir {
                return WalkState::Continue;
            }

            if entry.path_is_symlink() {
                let Ok(target) = fs::read_link(path.as_std_path()) else {
                    complete_flag.store(false, Ordering::Relaxed);
                    return WalkState::Continue;
                };
                let bytes = target.as_os_str().as_encoded_bytes();

                // A link inside the captured tree can point at a file Cargo reads outside it. The
                // target spelling alone says nothing about that file's bytes, so do not reuse any
                // outcome when it happens. Dangling links have no referent bytes yet; their
                // spelling and own timestamp are the complete input until one appears.
                if let Some(boundary) = &boundary {
                    match fs::canonicalize(path.as_std_path()) {
                        Ok(referent) => match Utf8PathBuf::from_path_buf(referent) {
                            Ok(referent) if referent.starts_with(boundary) => {
                                let relative = referent.strip_prefix(boundary).expect("the prefix was checked");

                                if excluded.iter().any(|excluded| relative.starts_with(excluded)) {
                                    complete_flag.store(false, Ordering::Relaxed);
                                }
                            }
                            Ok(_) | Err(_) => complete_flag.store(false, Ordering::Relaxed),
                        },
                        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_unresolved) => complete_flag.store(false, Ordering::Relaxed),
                    }
                }

                let file = SnapshotFile {
                    path: relative.to_path_buf(),
                    digest: digest(bytes),
                    size: bytes.len() as u64,
                    modified: symlink_modified_at(&path, native_timestamp),
                };

                files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(file);

                return WalkState::Continue;
            }

            match fs::metadata(path.as_std_path()) {
                Ok(metadata) if metadata.is_file() => {}
                _other => {
                    complete_flag.store(false, Ordering::Relaxed);
                    return WalkState::Continue;
                }
            }

            let Ok(bytes) = fs::read(path.as_std_path()) else {
                complete_flag.store(false, Ordering::Relaxed);
                return WalkState::Continue;
            };

            let file = SnapshotFile {
                path: relative.to_path_buf(),
                digest: digest(&bytes),
                size: bytes.len() as u64,
                // Read after the bytes rather than before them, so that a write landing between
                // the two leaves a time ahead of the content it describes rather than behind it.
                // The stale direction would be dangerous: a recorded time older than the bytes
                // recorded beside it would match a later capture of those same bytes.
                modified: modified_at(&path, native_timestamp),
            };

            files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(file);

            WalkState::Continue
        })
    });

    // Every worker thread has joined by the time `run` returns, so this ordinary load sees every
    // relaxed store a worker made; no stronger ordering is needed for a flag that only ever moves
    // from true to false.
    *complete = complete_flag.load(Ordering::Relaxed);

    let mut files = files.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);

    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

/// A file's modification time in nanoseconds since the epoch, where the platform reports one.
///
/// Every failure — an unsupported filesystem, a time before the epoch, a value past what fits —
/// answers "unknown" rather than a wrong number, because a wrong time is worse than none: it would
/// either invalidate a record that is sound or, in the other direction, be equal across a change.
fn modified_at(path: &Utf8Path, query: impl FnOnce(&Utf8Path, bool) -> std::io::Result<std::time::SystemTime>) -> Option<u64> {
    modified(query(path, true))
}

/// A symlink's own modification time, without following its target.
fn symlink_modified_at(path: &Utf8Path, query: impl FnOnce(&Utf8Path, bool) -> std::io::Result<std::time::SystemTime>) -> Option<u64> {
    modified(query(path, false))
}

/// Preserves the caller's link-versus-target choice through the native timestamp boundary.
pub(super) fn native_timestamp(path: &Utf8Path, follow: bool) -> std::io::Result<std::time::SystemTime> {
    timestamp_query(path, follow)
}

#[cfg(test)]
thread_local! {
    static TIMESTAMP_VALUES: std::cell::Cell<Option<(std::time::SystemTime, std::time::SystemTime)>> = const { std::cell::Cell::new(None) };
}

/// Uses current-thread link/target values when supplied, otherwise querying native metadata.
fn timestamp_query(path: &Utf8Path, follow: bool) -> std::io::Result<std::time::SystemTime> {
    #[cfg(test)]
    if let Some((link, target)) = TIMESTAMP_VALUES.get() {
        return Ok(if follow { target } else { link });
    }
    timestamp_query_using(
        path,
        // #[gamma::skip(bool_expr.negate, reason = "This thin native metadata adapter selects follow versus symlink_metadata. Distinguishing its exact follow argument requires a real symlink/host metadata fixture; owned operation-selection policy is tested separately.", tag = "native-boundary")]
        follow,
        |path| fs::metadata(path),
        |path| fs::symlink_metadata(path),
    )
}

/// Separates metadata-operation selection from native queries and timestamp conversion.
fn timestamp_query_using(
    path: &Utf8Path,
    follow: bool,
    metadata: impl FnOnce(&Utf8Path) -> std::io::Result<fs::Metadata>,
    symlink_metadata: impl FnOnce(&Utf8Path) -> std::io::Result<fs::Metadata>,
) -> std::io::Result<std::time::SystemTime> {
    let metadata = if follow { metadata(path) } else { symlink_metadata(path) }?;
    metadata.modified()
}

// #[gamma::skip(all, reason = "filesystem timestamp precision and range are platform-defined; adjacent nanoseconds and overflow values are not portably observable")]
fn modified(timestamp: std::io::Result<std::time::SystemTime>) -> Option<u64> {
    let since = timestamp.ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;

    u64::try_from(since.as_nanos()).ok()
}

/// Workspace-relative paths the snapshot never traverses.
// #[gamma::skip(all, reason = "the default exclusions and normalized caller exclusions are asserted as a set; sort/dedup mutations only expose platform traversal order")]
fn exclusions(root: &Utf8Path, excluded: &[Utf8PathBuf]) -> Vec<Utf8PathBuf> {
    let mut paths = vec![Utf8PathBuf::from("target"), Utf8PathBuf::from(".git")];

    for path in excluded {
        if path.is_relative() {
            paths.push(path.clone());
        } else if let Ok(relative) = path.strip_prefix(root)
            && !relative.as_str().is_empty()
        {
            paths.push(relative.to_path_buf());
        }
    }

    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};

    use camino::Utf8Path;

    use super::{TIMESTAMP_VALUES, WorkspaceSnapshot, digest, native_timestamp, timestamp_query_using};

    #[test]
    fn oct25_timestamp_query_selects_the_actual_metadata_operation() {
        let path = Utf8Path::new("synthetic-timestamp");
        for follow in [false, true] {
            let called = std::cell::Cell::new(None);
            let failure = timestamp_query_using(
                path,
                follow,
                |seen| {
                    assert_eq!(seen, path);
                    called.set(Some(true));
                    Err(std::io::ErrorKind::PermissionDenied.into())
                },
                |seen| {
                    assert_eq!(seen, path);
                    called.set(Some(false));
                    Err(std::io::ErrorKind::NotFound.into())
                },
            )
            .unwrap_err();
            assert_eq!(called.get(), Some(follow));
            assert_eq!(
                failure.kind(),
                if follow {
                    std::io::ErrorKind::PermissionDenied
                } else {
                    std::io::ErrorKind::NotFound
                }
            );
        }
    }

    #[test]
    fn recovered_native_timestamp_adapter_keeps_following_and_nonfollowing_queries_distinct() {
        struct Reset(Option<(std::time::SystemTime, std::time::SystemTime)>);
        impl Drop for Reset {
            fn drop(&mut self) {
                TIMESTAMP_VALUES.set(self.0);
            }
        }
        let link = std::time::UNIX_EPOCH + core::time::Duration::from_nanos(101);
        let target = std::time::UNIX_EPOCH + core::time::Duration::from_nanos(307);
        let _reset = Reset(TIMESTAMP_VALUES.replace(Some((link, target))));
        let path = Utf8Path::new("never-inspected-native-timestamp");
        assert_eq!(native_timestamp(path, false).unwrap(), link);
        assert_eq!(native_timestamp(path, true).unwrap(), target);
        assert_eq!(crate::discover::acceptance::native_timestamp(path, false).unwrap(), link);
        assert_eq!(crate::discover::acceptance::native_timestamp(path, true).unwrap(), target);
    }

    #[test]
    fn survivor_snapshot_timestamp_queries_preserve_controlled_values_and_return_none_for_errors() {
        let file = Utf8Path::new("workspace/source.rs");
        for (time, expected) in [
            (UNIX_EPOCH, Some(0)),
            (UNIX_EPOCH + Duration::from_nanos(200), Some(200)),
            (UNIX_EPOCH - Duration::from_secs(1), None),
            (UNIX_EPOCH + Duration::from_secs(u64::MAX / 1_000_000_000 + 1), None),
        ] {
            assert_eq!(
                super::modified_at(file, |path, follow| {
                    assert_eq!(path, file);
                    assert!(follow);
                    Ok(time)
                }),
                expected
            );
            assert_eq!(
                super::symlink_modified_at(file, |path, follow| {
                    assert_eq!(path, file);
                    assert!(!follow);
                    Ok(time)
                }),
                expected
            );
        }
        for kind in [std::io::ErrorKind::NotFound, std::io::ErrorKind::Unsupported] {
            assert_eq!(super::modified_at(file, |_, _| Err(kind.into())), None);
            assert_eq!(super::symlink_modified_at(file, |_, _| Err(kind.into())), None);
        }
    }

    #[test]
    fn survivor_identical_incomplete_workspace_snapshots_never_authorize_cache_reuse() {
        let snapshot = WorkspaceSnapshot {
            files: vec![super::SnapshotFile {
                path: "normal.rs".into(),
                digest: digest(b"fn f() {}\n"),
                size: 10,
                modified: Some(200),
            }],
            ..WorkspaceSnapshot::default()
        };
        assert!(!snapshot.is_complete());
        assert_eq!(snapshot, snapshot.clone());
        assert!(
            !snapshot.matches_snapshot(|| snapshot.clone()),
            "equality cannot override incomplete capture evidence"
        );
        let complete = WorkspaceSnapshot {
            complete: true,
            ..snapshot
        };
        assert!(complete.matches_snapshot(|| complete.clone()));
        let mut changed = complete.clone();
        changed.files[0].modified = Some(300);
        assert!(!complete.matches_snapshot(|| changed));
    }

    #[test]
    fn a_raw_bom_source_matches_its_normalized_discovery_generation() {
        let normalized = "fn source() {}\n";
        let retained = format!("{}{}", crate::parse::BOM, normalized);

        assert!(WorkspaceSnapshot::source_generation_matches(
            &digest(retained.as_bytes()),
            &digest(normalized.as_bytes()),
            Some(&retained)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn directory_and_broken_symlinks_form_a_repeatable_snapshot() {
        let directory = crate::testing::workdir("snapshot-symlinks-");
        let root = Utf8Path::from_path(directory.path()).expect("the temporary path is UTF-8");
        fs::create_dir(root.join("real")).expect("directory");
        std::os::unix::fs::symlink("real", root.join("directory-link")).expect("directory symlink");
        std::os::unix::fs::symlink("missing", root.join("broken-link")).expect("broken symlink");

        let snapshot = WorkspaceSnapshot::capture(root, &[]);

        assert!(snapshot.matches_current(root));
        assert_eq!(
            snapshot.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
            ["broken-link", "directory-link"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_an_external_referent_makes_reuse_ineligible() {
        let directory = crate::testing::workdir("snapshot-external-link-");
        let container = Utf8Path::from_path(directory.path()).expect("the temporary path is UTF-8");
        let root = container.join("workspace");
        let external = container.join("dependency.rs");

        fs::create_dir(&root).expect("workspace");
        fs::write(&external, "pub fn dependency() {}\n").expect("external input");
        std::os::unix::fs::symlink(&external, root.join("linked.rs")).expect("external link");

        let snapshot = WorkspaceSnapshot::capture(&root, &[]);

        assert!(
            !snapshot.is_complete(),
            "a link whose referent is outside the captured roots cannot certify a cache entry"
        );
    }

    #[test]
    fn an_ancestor_path_dependency_does_not_recapture_the_workspace() {
        let directory = crate::testing::workdir("snapshot-ancestor-dependency-");
        let container = Utf8Path::from_path(directory.path()).expect("the temporary path is UTF-8");
        let workspace = container.join("workspace");
        let dependency = container.join("dependency.rs");

        fs::create_dir(&workspace).expect("workspace");
        fs::write(workspace.join("source.rs"), "pub fn source() {}\n").expect("workspace input");
        fs::write(&dependency, "pub fn dependency() {}\n").expect("dependency input");

        let snapshot = WorkspaceSnapshot::capture_with_external(&workspace, &[], &[container.to_path_buf()], false);

        assert!(snapshot.is_complete());
        assert!(
            snapshot.external[0].files.iter().all(|file| !file.path.starts_with("workspace")),
            "the external half must not double-capture the workspace or its scratch outputs"
        );
        assert!(snapshot.matches_current(&workspace));

        fs::write(&dependency, "pub fn dependency() { panic!() }\n").expect("changed dependency");

        assert!(
            !snapshot.matches_current(&workspace),
            "a change outside the workspace must invalidate reuse"
        );
    }
}
#[test]
fn survivor_compilation_input_matrix_requires_both_complete_generations_and_exact_relevant_files() {
    let source = SnapshotFile {
        path: "src/lib.rs".into(),
        digest: "a".into(),
        size: 1,
        modified: Some(7),
    };
    let snapshot = WorkspaceSnapshot {
        complete: true,
        files: vec![source],
        ..WorkspaceSnapshot::default()
    };
    let roots = [Utf8PathBuf::from("src")];
    assert!(snapshot.matches_compilation_inputs(&snapshot, &roots));
    for (left_complete, right_complete) in [(false, false), (false, true), (true, false)] {
        let mut left = snapshot.clone();
        let mut right = snapshot.clone();
        left.complete = left_complete;
        right.complete = right_complete;
        assert!(!left.matches_compilation_inputs(&right, &roots));
    }
    let mut changed = snapshot.clone();
    changed.files[0].digest = "b".into();
    assert!(!snapshot.matches_compilation_inputs(&changed, &roots));
    assert!(snapshot.matches_compilation_inputs(&changed, &[Utf8PathBuf::from("other")]));
    assert!(!snapshot.matches_compilation_inputs(&changed, &[Utf8PathBuf::new()]));
    changed.external.push(ExternalInput {
        root: "dependency".into(),
        files: Vec::new(),
    });
    assert!(!snapshot.matches_compilation_inputs(&changed, &[Utf8PathBuf::from("other")]));
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain",
        "rust-toolchain.toml",
        ".cargo/config.toml",
    ] {
        assert!(WorkspaceSnapshot::is_compilation_input(Utf8Path::new(path), &[]), "{path}");
    }
    for path in ["README.md", "nested/.cargo", "xyzzy/config.toml"] {
        assert!(!WorkspaceSnapshot::is_compilation_input(Utf8Path::new(path), &[]), "{path}");
    }
    assert!(WorkspaceSnapshot::source_generation_matches("same", "same", None));
    assert!(WorkspaceSnapshot::source_generation_matches(
        &digest(b"original"),
        "normalized",
        Some("original")
    ));
    assert!(!WorkspaceSnapshot::source_generation_matches(
        "different",
        "normalized",
        Some("original")
    ));
}
