// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::error::Error as StdError;
use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, TryLockError};
use std::io::{Read as _, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, OnceLock};
use std::{env, io, thread};

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use cargo_gamma_process::{MemoryRequest, output as contained_output};
use walkdir::WalkDir;

use super::cargo_options::CargoOptions;
use super::config::Config;
use super::copy::{CopyOptions, VCS_DIRS, visible_vcs_metadata};
use super::events::Events;
#[cfg(test)]
use super::faults::{self, Fault};
use super::loader::{Launch, toolchain_libraries};
use super::manifest::{CAP_LINTS, Manifest, RUNTIME_CRATE, RUNTIME_PACKAGE, WorkspaceRuntimeFeatures, anchor_cargo_config, cap_lints};
use super::nextest::Harness;
use super::resources::{ResourcePermit, Resources};
use super::rustc_wrapper::{CAPTURE_DIR_VAR, ORIGINAL_WRAPPER_VAR, reset_capture_directory, wrapper_path};
use super::sync::sync_or_copy;
use super::test_binary::{TEST_THREADS_VAR, TestBinary, harness_threads};
use super::test_environment::TestEnvironment;
use crate::discover::TargetFile;
use crate::error::error;
use crate::{Result, cfg};

/// The workspace and optional redirected-cache locks claimed as one handoff unit.
pub(crate) struct CacheLocks {
    workspace: File,
    redirected: Option<File>,
    #[cfg(any(test, feature = "internals"))]
    identity: usize,
}

#[cfg(any(test, feature = "internals"))]
static NEXT_CACHE_LOCK_IDENTITY: AtomicUsize = AtomicUsize::new(1);

#[cfg(any(test, feature = "internals"))]
fn next_cache_lock_identity(next: &AtomicUsize) -> usize {
    next.fetch_add(1, Ordering::Relaxed)
}

/// Intercepts the first journal arming immediately before its native open and workspace installation.
/// The hook is consumed before invocation; callers establish any required state-publication
/// precondition themselves. This current-thread scope cannot nest and clears the hook on drop.
#[cfg(all(test, not(miri)))]
pub(super) fn with_journal_arm_hook<T>(hook: impl FnOnce(&Utf8Path) + 'static, operation: impl FnOnce() -> T) -> T {
    tests::with_journal_arm_hook(hook, operation)
}

/// Identifies one claimed lock pair across the adoption-to-preparation handoff.
#[cfg(any(test, feature = "internals"))]
pub(crate) const fn cache_lock_identity(locks: &CacheLocks) -> usize {
    locks.identity
}

/// Identifies the workspace allowed to reuse a cache directory.
const CACHE_OWNER: &str = ".cargo-gamma-owner";
const CAMPAIGN_LOCATION: &str = "campaign-location";

/// The private cache home propagated only by Cargo Gamma's own test harness.
const TEST_CACHE_HOME_VAR: &str = "CARGO_GAMMA_TEST_CACHE_HOME";

/// Generous marker bound allowing 32,767 Windows UTF-16 code units at four UTF-8 bytes each.
const MAX_CACHE_OWNER_LEN: u64 = 32_767 * 4;

/// A scratch copy of the workspace, instrumented and ready to build.
#[derive(Debug)]
pub struct Workspace {
    /// Where Cargo artifacts and reusable campaign state live.
    base: Utf8PathBuf,

    /// Root of the copied tree.
    pub(super) root: Utf8PathBuf,

    /// Where build artifacts go, kept outside the copied tree so that repeated runs are
    /// incremental rather than starting cold every time.
    pub(super) target: Utf8PathBuf,

    /// Directories a test binary needs on its dynamic loader path.
    ///
    /// `cargo test` sets this before running a binary it built; we run binaries ourselves, so we
    /// have to reproduce it for toolchains that link `std` dynamically.
    pub(super) libraries: Vec<Utf8PathBuf>,

    /// How cargo is invoked in this tree.
    pub(super) cargo: CargoOptions,

    /// Where the vendored guard runtime lives, so that a package can be linked to it at the moment
    /// its own mutants are known rather than all of them up front.
    runtime: Utf8PathBuf,

    /// Compiler invocations retained across every Cargo command in one build sequence.
    rustc_captures: Option<Utf8PathBuf>,

    /// Whether the current build sequence started with an empty capture generation.
    rustc_captures_ready: AtomicBool,

    /// Whether the tree survives the run for inspection.
    pub(super) leak: bool,

    /// Whether the run got far enough to be worth keeping build artifacts for.
    ///
    /// Artifacts are what make the next run incremental, so a run that reached the point of having
    /// something to measure leaves them behind on purpose. A run that failed before then leaves
    /// nothing a later run could reuse — only the object files of a tree that no longer exists,
    /// which on a large workspace is tens of gigabytes of dead weight on a disk that a CI job may
    /// well need for its next step.
    settled: AtomicBool,

    /// How this run's test binaries are executed, once the build has produced them.
    ///
    /// `None` is the default runner, which launches each binary directly.
    nextest: Option<Harness>,
    test_environment: Option<TestEnvironment>,
    cargo_metadata: Option<String>,

    /// The environment values that are the same for every test-binary launch of the whole run.
    ///
    /// Filled on first use rather than at construction because it is a property of running tests,
    /// and a workspace is also built for commands that never run one. See [`Launch`].
    launch: OnceLock<Launch>,

    /// How many threads each spawned test harness is told to use, or `None` to leave it alone.
    ///
    /// Carried here and set on each launched command rather than on this process's own
    /// environment. Setting it globally was correct for the real binary — one run, one thread, no
    /// children yet — and wrong for the test suite, which calls `run` from forty tests at once
    /// while every other thread in the process is reading the environment. `setenv` racing
    /// `getenv` is a data race in `libc`, not a confusing value, and there is nothing to restore
    /// afterwards that would fix it. Every launch shares this one answer, so the baseline and the
    /// sweep still cannot disagree about the width of the workload they measure and judge.
    harness_threads: OnceLock<Option<String>>,

    /// Shared resources declared by selected tests and their campaign-wide admission state.
    resources: Resources,

    testing_journal: Option<super::testing_state::TestingJournal>,
    #[cfg(test)]
    live_sweep_publisher: Option<fn(&Utf8Path, &str) -> Result<()>>,

    /// The private cache home inherited by nested Cargo Gamma processes in self-tests.
    #[cfg(any(test, feature = "internals"))]
    test_cache_home: Option<Utf8PathBuf>,

    /// Held for the life of the run so that another cargo-gamma command targeting the same original
    /// workspace is turned away. Released when the process ends, however it ends, so a crash cannot
    /// leave a lock nobody can clear.
    _workspace_lock: Option<File>,

    /// Held when reusable state was redirected, so another workspace cannot use the same cache.
    _cache_lock: Option<File>,

    /// Whether [`Workspace::teardown`] has already removed what the destructor would remove.
    ///
    /// The destructor is the fallback for the paths that never reach an explicit teardown — an
    /// error part-way through a run, a panic, a test that drops the tree without ceremony — so it
    /// has to stay. This is what keeps it from walking a tree that is already gone.
    torn_down: bool,
}

/// Persisted launch state, decoded before cache admission or journal recovery.
#[derive(Debug, serde::Deserialize)]
struct TestingSnapshot {
    base: Utf8PathBuf,
    root: Utf8PathBuf,
    target: Utf8PathBuf,
    runtime: Utf8PathBuf,
    libraries: Vec<Utf8PathBuf>,
    cargo: CargoOptions,
    nextest: Option<Harness>,
    test_environment: Option<TestEnvironment>,
    launch: Launch,
    harness_threads: Option<String>,
    resources: Resources,
    #[serde(default)]
    scheduler_version: Option<u32>,
}

/// I/O needed to resume a prepared workspace, separate from decoding and hydration.
pub(crate) struct TestingResumeIo<
    S = fn(&Utf8Path, Option<&Utf8Path>) -> Utf8PathBuf,
    C = fn(&Utf8Path, &Utf8Path) -> Result<File>,
    J = fn(&Utf8Path, Option<u64>) -> Result<super::testing_state::TestingJournal>,
> {
    default_scratch: S,
    claim_cache: C,
    open_journal: J,
}

impl TestingResumeIo {
    pub(crate) const FILESYSTEM: Self = Self {
        default_scratch: gamma_base,
        claim_cache: claim_redirected_cache,
        open_journal: super::testing_state::TestingJournal::open,
    };
}

impl TestingSnapshot {
    fn resume_with<C, J>(
        snapshot: serde_json::Value,
        source: &Utf8Path,
        base: &Utf8Path,
        default_scratch: impl FnOnce() -> Utf8PathBuf,
        claim_cache: impl FnOnce(&Utf8Path, &Utf8Path) -> Result<C>,
        open_journal: impl FnOnce(&Utf8Path) -> Result<J>,
    ) -> Result<(Self, Option<C>, J)> {
        let saved: Self = serde_json::from_value(snapshot)
            .map_err(|cause| error!("prepared testing workspace is invalid; start a new campaign").caused_by(cause))?;
        if saved.scheduler_version != Some(2) {
            return Err(error!(
                "prepared testing workspace uses a different scheduler; start a new campaign without --resume-testing to use exact-first adaptive testing"
            ));
        }
        let scratch = saved
            .root
            .parent()
            .ok_or_else(|| error!("prepared testing workspace has no scratch directory"))?;
        let default_scratch = default_scratch();
        if saved.base != base
            || saved.target != base.join("target")
            || (scratch != default_scratch && scratch != base)
            || saved.root != scratch.join("workspace")
            || saved.runtime != scratch.join("rt")
        {
            return Err(error!(
                "prepared testing workspace does not belong to `{source}` and `{base}`; start a new campaign"
            ));
        }
        let cache_lock = if scratch == base && base != default_scratch {
            Some(claim_cache(source, base)?)
        } else {
            None
        };
        let journal = open_journal(base)?;
        Ok((saved, cache_lock, journal))
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if
        // #[gamma::skip(cond.always_false, reason = "This is Drop's torn_down guard. Removing it only calls self.teardown(); teardown immediately checks the identical self.torn_down and returns Ok(()) before any effect. Already-torn-down drop and all not-torn-down paths are unchanged.", tag = "equivalent")]
        self.torn_down {
            return;
        }

        // The tree is rebuilt from scratch on the next run, so failing to remove it costs disk
        // rather than correctness, and there is no caller in a position to do anything about it.
        // A caller that wants to choose the moment, or to see what went wrong, calls `teardown`
        // instead; this is what happens to the runs that do not.
        let _torn_down = self.teardown();
    }
}

impl Workspace {
    /// Removes the scratch tree now, and says what went wrong if anything did.
    ///
    /// The destructor does this too, but it does it at a moment nobody chose and reports nothing.
    /// On a real run the tree is a full copy of the workspace plus its build artifacts —
    /// gigabytes, thousands of files, and a walk of every one of them — and that walk runs after
    /// the report has been printed, so from outside it looks like a finished tool that will not
    /// exit. A caller that has somewhere better to put the wait, or that wants the failure on the
    /// error stream rather than discarded, calls this instead.
    ///
    /// Idempotent, and leaves nothing for the destructor to repeat. A tree kept by `--leak-dirs`
    /// is left where it is: the point of that flag is that the directory is still there afterwards.
    ///
    /// # Errors
    ///
    /// Returns an error if the tree, or the build directory of a run that never settled, could not
    /// be removed. The next run rebuilds both from scratch, so this costs disk rather than
    /// correctness — which is why the destructor can afford to swallow it and a caller can afford
    /// to report it and carry on.
    pub fn teardown(&mut self) -> Result<()> {
        if self.torn_down {
            return Ok(());
        }

        self.torn_down = true;

        if self.leak {
            return Ok(());
        }

        let tree = if self.settled.load(Ordering::Relaxed) {
            // A settled run leaves the tree behind for delta synchronization on the next run.
            // The sentinel written at copy/sync time remains, so the next prepare can reuse it.
            Ok(())
        } else {
            remove_tree(&self.root)
        };

        // Artifacts are what make the next run incremental, so a run that got far enough to have
        // something to measure leaves them behind on purpose.
        let build = if self.settled.load(Ordering::Relaxed) {
            Ok(())
        } else {
            remove_tree(&self.target)
        };

        tree.and(build)
    }

    /// Says where to look at the tree, for an error that wants the reader to go and read it.
    ///
    /// The tree is deleted when the run ends unless `--leak-dirs` was given, so naming its path
    /// unconditionally sends the reader to a directory that no longer exists by the time they get
    /// there — which reads as a second, phantom bug on top of the one being reported.
    pub(super) fn inspect_hint(&self) -> String {
        if self.leak {
            format!("The tree is at `{}` if you want to look.", self.root)
        } else {
            "Re-run with `--leak-dirs` to keep the instrumented tree and look at it.".to_owned()
        }
    }

    /// Copies and instruments the tree.
    #[cfg(test)]
    pub(super) fn prepare(source: &Utf8Path, target: &Utf8Path, config: &Config, events: &mut impl Events) -> Result<Self> {
        Self::prepare_with_locks(source, target, config, events, None)
    }

    /// Copies and instruments the tree while retaining locks taken before cache adoption.
    pub(super) fn prepare_with_locks(
        source: &Utf8Path,
        target_directory: &Utf8Path,
        config: &Config,
        events: &mut impl Events,
        locks: Option<CacheLocks>,
    ) -> Result<Self> {
        Self::prepare_with_locks_observed(source, target_directory, config, events, locks, |_| {})
    }

    fn prepare_with_locks_observed(
        source: &Utf8Path,
        target_directory: &Utf8Path,
        config: &Config,
        events: &mut impl Events,
        locks: Option<CacheLocks>,
        observe_locks: impl FnOnce(&CacheLocks),
    ) -> Result<Self> {
        config.cargo.validate()?;

        // Absolute from here on, so that the copy's own exclusion — which compares the scratch base
        // against the paths it walks — is comparing two paths of the same kind.
        let source = &absolute(source);
        let scratch = gamma_base(source, config.cache_dir.as_deref());
        let base = campaign_base(source, target_directory, config.cache_dir.as_deref());

        if config.cache_dir.is_none() {
            validate_default_cache_base(source, &scratch)?;
            validate_default_campaign_base(source, target_directory, &base)?;
        }
        ensure_copy_layout_terminates(source, &scratch, &base)?;

        let root = scratch.join("workspace");
        if config.cache_dir.is_some() {
            ensure_vcs_visibility(source, &root)?;
        }
        let target = base.join("target");
        let runtime = scratch.join("rt");

        events.begin("Copying", "Copied", "the workspace");

        fs::create_dir_all(scratch.as_std_path())
            .map_err(|cause| error!("could not create the scratch directory at `{scratch}`").caused_by(cause))?;
        fs::create_dir_all(base.as_std_path())
            .map_err(|cause| error!("could not create the campaign cache at `{base}`").caused_by(cause))?;

        let locks = match locks {
            Some(locks) => locks,
            None => claim_cache(source, target_directory, config.cache_dir.as_deref())?,
        };
        observe_locks(&locks);
        #[cfg(any(test, feature = "internals"))]
        crate::testing::pause_during_workspace_preparation(source, cache_lock_identity(&locks));
        let CacheLocks {
            workspace: workspace_lock,
            redirected: cache_lock,
            #[cfg(any(test, feature = "internals"))]
                identity: _identity,
        } = locks;

        events.testing_log(&base)?;

        let _outcome = sync_or_copy(
            source,
            &root,
            &base,
            CopyOptions {
                copy_ignored: config.copy_ignored,
            },
        )?;

        if config.cache_dir.is_none() {
            expose_vcs_metadata(source, &root)?;
        }
        vendor_runtime(&runtime)?;
        anchor_manifests(source, &root, &runtime)?;

        let libraries = toolchain_libraries(&root, &target);

        let rustc_captures = rustc_capture_directory(&root, &target);
        let workspace = Self {
            base,
            root,
            target,
            libraries,
            cargo: config.cargo.clone(),
            runtime,
            rustc_captures,
            rustc_captures_ready: AtomicBool::new(false),
            leak: config.leak_dirs,
            settled: AtomicBool::new(false),
            nextest: None,
            test_environment: None,
            cargo_metadata:
            // #[gamma::skip(option.none_to_some, reason = "Producer-domain equivalence of only prepare_with_locks's initial private cargo_metadata field: its type is Option<String>. The sole production read is arm_nextest. The sole production caller of arm_nextest is measure::arm_selected_runner, which first calls work.arm_test_environment(...)?, and that method unconditionally assigns Some(fresh metadata) before returning Ok. If preparation fails, arm_nextest is not called and the workspace is dropped. Other paths do not read or serialize this field; testing snapshots deliberately omit it. Thus None versus Some(empty String) is always overwritten before the only reachable read for this constructor. The distinct adopt/resume constructor fields, including tests that deliberately call arm_nextest before environment preparation, are not mutated and are not included in the proof.", tag = "equivalent")]
            None,
            launch: OnceLock::new(),
            harness_threads: OnceLock::new(),
            resources: Resources::default(),
            testing_journal: None,
            #[cfg(test)]
            live_sweep_publisher: None,
            #[cfg(any(test, feature = "internals"))]
            test_cache_home: crate::testing::cache_home(source),
            _workspace_lock: None,
            _cache_lock: None,
            torn_down: false,
        };
        let workspace = Self::retain_prepared_locks(workspace, workspace_lock, cache_lock);

        events.end("");

        Ok(workspace)
    }

    #[expect(
        clippy::used_underscore_binding,
        reason = "these ownership-only handles are retained until workspace destruction"
    )]
    fn retain_cache_locks(&mut self, workspace_lock: File, cache_lock: Option<File>) {
        self._workspace_lock = Some(workspace_lock);
        self._cache_lock = cache_lock;
    }

    fn retain_prepared_locks(mut workspace: Self, workspace_lock: File, cache_lock: Option<File>) -> Self {
        workspace.retain_cache_locks(workspace_lock, cache_lock);
        workspace
    }

    /// Adds the guard runtime as a dependency of one package.
    ///
    /// Called when a package is about to be instrumented, not when the tree is copied: which
    /// packages need it is not known until they have been scanned, and a package that turned out
    /// to have no mutants should not carry a dependency it never uses.
    pub(super) fn link_runtime(&self, package: &str, files: &[TargetFile]) -> Result<()> {
        let runtime = &self.runtime;
        let Some(path) = self.manifest_of(package, files) else {
            return Ok(());
        };

        let mut manifest = Manifest::read(&path)?;

        manifest.link_runtime(runtime)?;
        manifest.save()
    }

    /// Locates a package's manifest inside the copied tree.
    fn manifest_of(&self, package: &str, files: &[TargetFile]) -> Option<Utf8PathBuf> {
        let file = files.iter().find(|file| file.package == package)?;
        let mut directory = self.root.join(&file.path);
        let real_root = physical(&self.root);

        // Walk up from a source file until a manifest appears, which is the package root.
        while directory.pop() {
            let candidate = directory.join("Cargo.toml");

            // `is_file` resolves every component, so a copied symlink pointing outside the scratch
            // tree would answer for the user's real manifest — which `Manifest::save` then rewrites.
            // The physical path is what decides, because that is where the write lands.
            if candidate.as_std_path().is_file() && physical(&candidate).starts_with(&real_root) {
                return Some(candidate);
            }

            if directory == self.root {
                break;
            }
        }

        None
    }

    /// Replaces a file that the copy already put in the tree.
    ///
    /// Refuses to follow a symlink or to create a file that was not copied, because either means
    /// writing somewhere the copy did not choose — through a link, that is somewhere outside the
    /// scratch tree entirely, and the tree holds a copy of the user's real source.
    /// Returns whether the file had to be written, so a caller can report what it really changed.
    ///
    /// `root` is the scratch tree the write must land inside. `symlink_metadata` is non-following
    /// for the last component alone, so it cannot see a link among the *intermediate* ones; the copy
    /// recreates links verbatim, including absolute targets, so such a prefix is reachable. The
    /// physical path is therefore checked as well.
    pub(super) fn overwrite(root: &Utf8Path, path: &Utf8Path, contents: &str) -> Result<bool> {
        let metadata = fs::symlink_metadata(path.as_std_path())
            .map_err(|cause| error!("could not write `{path}`, which the copy did not create").caused_by(cause))?;

        if !metadata.is_file() {
            return Err(error!(
                "refusing to write `{path}`, which is a link or a device rather than the copied source file"
            ));
        }

        let (real_path, real_root) = (physical(path), physical(root));

        if !real_path.starts_with(&real_root) {
            return Err(error!(
                "refusing to write `{path}`, which is `{real_path}` — outside the scratch tree at `{real_root}`, \
                 so the write would land in the real source tree"
            ));
        }

        // Cargo decides what to recompile from mtime, not from content, so writing a file back
        // byte-for-byte still rebuilds its crate and everything downstream of it. The rollback loop
        // rewrites the whole tree every round while changing only the few files whose mutants were
        // withdrawn, which made every round cost a full workspace build. Comparing first turns the
        // untouched majority into a read.
        if let Ok(existing) = fs::read(path.as_std_path())
            && existing == contents.as_bytes()
        {
            return Ok(false);
        }

        fs::write(path.as_std_path(), contents).map_err(|cause| error!("could not write `{path}`").caused_by(cause))?;

        Ok(true)
    }

    /// Wraps a root as a workspace, without copying, querying storage, or locking anything.
    ///
    /// The root may be synthetic for command/policy tests, or an owned real tree for native
    /// acceptance. It is left in place on drop because this handle never owns its storage.
    #[cfg(any(test, feature = "internals"))]
    #[must_use]
    pub fn adopt(root: Utf8PathBuf, target: Utf8PathBuf) -> Self {
        let test_cache_home = crate::testing::cache_home(&root);
        let base = target.parent().unwrap_or(&target).to_owned();

        Self {
            base,
            runtime: root.join("gamma-rt"),
            root,
            target,
            rustc_captures: None,
            rustc_captures_ready: AtomicBool::new(false),
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            nextest: None,
            test_environment: None,
            cargo_metadata: None,
            settled: AtomicBool::new(true),
            leak: true,
            launch: OnceLock::new(),
            harness_threads: OnceLock::new(),
            resources: Resources::default(),
            testing_journal: None,
            #[cfg(test)]
            live_sweep_publisher: None,
            #[cfg(any(test, feature = "internals"))]
            test_cache_home,
            _workspace_lock: None,
            _cache_lock: None,
            torn_down: false,
        }
    }

    /// Replaces the arguments every test binary is launched with.
    ///
    /// Only for tests that stand a shell in for a compiled harness, where the script to run is
    /// itself an argument.
    #[cfg(any(test, feature = "internals"))]
    pub(crate) fn set_test_args(&mut self, args: Vec<String>) {
        self.cargo.test_args = args;
    }

    pub(super) fn set_resources(&mut self, resources: Resources) {
        self.resources = resources;
    }

    pub(crate) fn reporting_profile(&self) -> Option<&str> {
        self.cargo.profile.as_deref()
    }

    pub(super) fn testing_snapshot(&self) -> Result<serde_json::Value> {
        #[derive(serde::Serialize)]
        struct Snapshot<'a> {
            base: &'a Utf8Path,
            root: &'a Utf8Path,
            target: &'a Utf8Path,
            runtime: &'a Utf8Path,
            libraries: &'a [Utf8PathBuf],
            cargo: &'a CargoOptions,
            nextest: &'a Option<Harness>,
            test_environment: &'a Option<TestEnvironment>,
            launch: &'a Launch,
            harness_threads: Option<&'a str>,
            resources: &'a Resources,
            #[serde(skip_serializing_if = "Option::is_none")]
            scheduler_version: Option<u32>,
        }
        serde_json::to_value(Snapshot {
            base: &self.base,
            root: &self.root,
            target: &self.target,
            runtime: &self.runtime,
            libraries: &self.libraries,
            cargo: &self.cargo,
            nextest: &self.nextest,
            test_environment: &self.test_environment,
            launch: self.launch(),
            harness_threads: self.harness_threads(),
            resources: &self.resources,
            scheduler_version: Some(2),
        })
        .map_err(|cause| error!("could not encode the prepared testing workspace").caused_by(cause))
    }

    pub(super) fn testing_roots(&self) -> Result<Vec<Utf8PathBuf>> {
        let mut paths =
        // #[gamma::skip(collection.reverse_vec, reason = "Workspace::testing_roots unconditionally sorts and deduplicates paths after seeding root/runtime; reversing only their initial insertion order produces the same roots.", tag = "equivalent")]
        vec![self.root.clone(), self.runtime.clone()];
        if let Some(nextest) = &self.nextest {
            paths.extend(nextest.testing_paths().into_iter().map(Utf8Path::to_owned));
        }
        if let Some(environment) = &self.test_environment {
            paths.extend(environment.testing_paths()?);
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    pub(crate) fn resume_testing<C, J>(
        source: &Utf8Path,
        base: &Utf8Path,
        snapshot: serde_json::Value,
        workspace_lock: impl Into<Option<File>>,
        valid_bytes: u64,
        io: TestingResumeIo<
            impl FnOnce(&Utf8Path, Option<&Utf8Path>) -> Utf8PathBuf,
            impl FnOnce(&Utf8Path, &Utf8Path) -> Result<C>,
            impl FnOnce(&Utf8Path, Option<u64>) -> Result<J>,
        >,
    ) -> Result<Self>
    where
        C: Into<Option<File>>,
        J: Into<Option<super::testing_state::TestingJournal>>,
    {
        let (saved, cache_lock, journal) = TestingSnapshot::resume_with(
            snapshot,
            source,
            base,
            || (io.default_scratch)(source, None),
            io.claim_cache,
            |base| (io.open_journal)(base, Some(valid_bytes)),
        )?;
        Ok(Self {
            base: saved.base,
            root: saved.root,
            target: saved.target,
            runtime: saved.runtime,
            libraries: saved.libraries,
            cargo: saved.cargo,
            nextest: saved.nextest,
            test_environment: saved.test_environment,
            resources: saved.resources,
            testing_journal: journal.into(),
            #[cfg(test)]
            live_sweep_publisher: None,
            launch: OnceLock::from(saved.launch),
            harness_threads: OnceLock::from(saved.harness_threads),
            rustc_captures: None,
            rustc_captures_ready: AtomicBool::new(false),
            cargo_metadata: None,
            settled: AtomicBool::new(true),
            leak: false,
            #[cfg(any(test, feature = "internals"))]
            test_cache_home: crate::testing::cache_home(source),
            _workspace_lock: workspace_lock.into(),
            _cache_lock: cache_lock.and_then(Into::into),
            torn_down: false,
        })
    }

    pub(super) fn arm_testing_journal(&mut self) -> Result<()> {
        #[cfg(all(test, not(miri)))]
        tests::before_journal_arm(&self.base);
        self.testing_journal = Some(super::testing_state::TestingJournal::open(&self.base, None)?);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn set_journal_for_test(&mut self, journal: super::testing_state::TestingJournal) {
        self.testing_journal = Some(journal);
    }

    #[cfg(test)]
    pub(super) fn set_live_sweep_publisher_for_test(&mut self, publish: fn(&Utf8Path, &str) -> Result<()>) {
        self.live_sweep_publisher = Some(publish);
    }

    pub(super) fn retain_sweep_cost(&mut self, mut cost: Option<super::session::SweepCost>) {
        if let Some(journal) = &mut self.testing_journal {
            journal.prior_confirmations = cost.as_mut().map_or_else(std::collections::BTreeSet::new, |cost| {
                core::mem::take(&mut cost.confirmed_initial_killers)
            });
            journal.previous_sweep = cost;
        }
    }

    pub(super) fn retain_initial_hints(&mut self, hints: Option<super::session::InitialHints>) {
        if let Some(journal) = &mut self.testing_journal {
            journal.initial_hints = hints;
        }
    }

    pub(super) const fn has_testing_journal(&self) -> bool {
        self.testing_journal.is_some()
    }

    pub(super) fn initial_hints(&self) -> Option<&super::session::InitialHints> {
        self.testing_journal.as_ref().and_then(|journal| journal.initial_hints.as_ref())
    }

    pub(super) fn prior_initial_confirmations(&self) -> std::collections::BTreeSet<crate::model::MutantId> {
        self.testing_journal
            .as_ref()
            .map_or_else(std::collections::BTreeSet::new, |journal| journal.prior_confirmations.clone())
    }

    pub(super) fn with_sweep_diagnostics(&self, cost: Option<super::session::SweepCost>) -> Option<super::session::SweepCost> {
        let failures = self
            .testing_journal
            .as_ref()
            .map_or(0, |journal| journal.live_refresh_failures.load(Ordering::Relaxed));
        Self::sweep_diagnostics(cost, failures)
    }

    fn sweep_diagnostics(mut cost: Option<super::session::SweepCost>, failures: usize) -> Option<super::session::SweepCost> {
        if let Some(accounting) = cost.as_mut().and_then(|cost| cost.accounting.as_mut()) {
            accounting.live_refresh_failures = accounting.live_refresh_failures.saturating_add(failures);
        }
        cost
    }

    pub(super) fn checkpoint_sweep(&self, cost: &super::session::SweepCost) -> Result<()> {
        if let Some(journal) = &self.testing_journal {
            let cumulative = self
                .with_sweep_diagnostics(super::session::SweepCost::combine_checkpoint(
                    journal.previous_sweep.clone(),
                    Some(cost.clone()),
                ))
                .expect("a supplied sweep cost always produces a combined cost");
            journal.publish_cost(cumulative.clone())?;
            self.refresh_live_sweep(journal, cumulative)?;
        }
        Ok(())
    }

    fn refresh_live_sweep(&self, journal: &super::testing_state::TestingJournal, cumulative: super::session::SweepCost) -> Result<()> {
        let refreshed = self.write_live_sweep(&cumulative);
        Self::record_live_refresh(&journal.live_refresh_failures, cumulative, refreshed, |cost, detail| {
            journal.publish_diagnostic(cost, detail)
        })
    }

    /// Live diagnostics are derived output; a failed refresh is recoverable only after the durable
    /// journal records that failure. Losing the journal write must still stop execution.
    fn record_live_refresh(
        failures: &AtomicUsize,
        mut cost: super::session::SweepCost,
        refreshed: Result<()>,
        publish: impl FnOnce(super::session::SweepCost, String) -> Result<()>,
    ) -> Result<()> {
        if let Err(failure) = refreshed {
            let _updated = failures.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| Some(count.saturating_add(1)));
            let accounting = cost.accounting.get_or_insert_default();
            accounting.live_refresh_failures = accounting.live_refresh_failures.saturating_add(1);
            publish(cost, format!("live sweep diagnostics refresh failed: {failure}"))?;
        }
        Ok(())
    }

    fn write_live_sweep(&self, cost: &super::session::SweepCost) -> Result<()> {
        let path = self.base.join("gamma-sweep-live.json");
        let contents =
        // #[gamma::skip(try.propagate_to_unwrap, reason = "SweepCost::live_json serializes a derived Live DTO containing Option<&SweepAccounting>,u32,u64 into String. SweepAccounting and nested AttemptCost contain only derived bool/Option<usize>/usize/u64 fields. No user Serialize or external writer exists, keys are fixed struct fields, so serialization cannot return Err absent allocation abort; panic_on_failure is inactive.", tag = "equivalent")]
        cost
            .live_json()
            .map_err(|cause| error!("could not encode live sweep diagnostics").caused_by(cause))?;
        #[cfg(test)]
        if let Some(publish) = self.live_sweep_publisher {
            return publish(&path, &contents);
        }
        crate::elements::write(&path, &contents)
    }
    pub(super) fn checkpoint_mutant(
        &self,
        mutant: &crate::model::Mutant,
        killers: &super::killers::Killers,
        cost: &super::session::SweepCost,
    ) -> Result<()> {
        if let Some(journal) = &self.testing_journal {
            let cumulative = self.with_sweep_diagnostics(super::session::SweepCost::combine_checkpoint(
                journal.previous_sweep.clone(),
                Some(cost.clone()),
            ));
            journal.publish_with_cost(mutant, killers, cumulative.clone())?;
            if let Some(cumulative) = cumulative {
                self.refresh_live_sweep(journal, cumulative)?;
            }
        }
        Ok(())
    }

    pub(super) fn complete_testing(&self) -> Result<()> {
        if self.testing_journal.is_some() {
            super::testing_state::TestingJournal::complete(&self.base)?;
        }
        Ok(())
    }

    pub(super) fn acquire_resources<'work>(&'work self, binary: &TestBinary, only: super::verdict::Only<'_>) -> ResourcePermit<'work> {
        self.resources.acquire(binary, only)
    }

    pub(super) fn required_resources(&self, binary: &TestBinary, only: super::verdict::Only<'_>) -> Vec<Arc<str>> {
        self.resources.required(binary, only)
    }

    pub(super) fn resource_capacities(&self) -> crate::HashMap<Arc<str>, usize> {
        self.resources.capacities()
    }

    pub(super) fn cargo(&self) -> Command {
        let mut command = Command::new(cargo_binary());

        let _ = command.current_dir(self.root.as_std_path());
        let _ = command.env("CARGO_TARGET_DIR", self.target.as_std_path());
        let _ = command.env("CARGO_TERM_COLOR", if self.cargo.color { "always" } else { "never" });
        self.inherit_test_cache_home(&mut command);

        cap_ambient_rustflags(&mut command);

        // A guard is inert unless this names its mutant. Proc macros run inside the compiler, so a
        // live mutant in one executes during the build rather than during a test — an infinite loop
        // there hangs the one build the whole run depends on, with no test to time it out. The
        // variable is scrubbed rather than trusted to be absent, since test processes set it and a
        // user debugging a mutant by hand may export it.
        let _ = command.env_remove(gamma_rt::ACTIVE_VAR);

        if self.rustc_captures_ready.load(Ordering::Acquire)
            && let Some(captures) = &self.rustc_captures
            && let Some(wrapper) = wrapper_path()
            && let cfg::RustcWrapperChain::Interpose(original) = cfg::rustc_wrapper_chain(&self.root)
        {
            let _ = command.env(CAPTURE_DIR_VAR, captures.as_std_path());

            if let Some(original) = original.filter(|candidate| !wrapper_is_self(candidate, &wrapper, &self.root)) {
                let _ = command.env(ORIGINAL_WRAPPER_VAR, original);
            } else {
                let _ = command.env_remove(ORIGINAL_WRAPPER_VAR);
            }

            let _ = command.env("RUSTC_WRAPPER", wrapper.as_std_path());
        }

        command
    }

    /// The persistent compiler-capture directory, only when this process can populate it.
    pub(super) fn rustc_captures(&self) -> Option<Utf8PathBuf> {
        self.rustc_captures_ready
            .load(Ordering::Acquire)
            .then(|| self.rustc_captures.clone())
            .flatten()
    }

    /// Starts one build sequence without captures left by an earlier run.
    pub(super) fn begin_build_sequence(&self) {
        let ready = self.rustc_captures.as_deref().is_some_and(reset_capture_directory);
        self.rustc_captures_ready.store(ready, Ordering::Release);
    }

    /// Propagates the self-test cache boundary to Cargo, test binaries, and nested Gamma commands.
    #[cfg_attr(
        not(any(test, feature = "internals")),
        expect(
            clippy::unused_self,
            clippy::needless_pass_by_ref_mut,
            reason = "normal builds intentionally retain the same no-op adapter signature as self-test builds"
        )
    )]
    pub(super) fn inherit_test_cache_home(&self, command: &mut Command) {
        #[cfg(any(test, feature = "internals"))]
        crate::testing::inherit_cache_home(command, self.test_cache_home.as_deref());

        #[cfg(not(any(test, feature = "internals")))]
        let _ = command;
    }

    /// Marks the run as having got far enough that its build artifacts are worth keeping.
    ///
    /// Until this is called, dropping the tree also discards everything built for it: the artifacts
    /// of a run that never produced a result are of no use to the next one, and on a large
    /// workspace they can be tens of gigabytes.
    pub(super) fn settle(&self) {
        self.settled.store(true, Ordering::Relaxed);
    }

    /// Root of the copied tree, which is where a runner is pointed at the workspace.
    pub(crate) fn root(&self) -> &Utf8Path {
        &self.root
    }

    pub(crate) fn report_source_root(&self) -> Utf8PathBuf {
        self.base.join(".cargo-gamma-report-source")
    }

    pub(crate) fn snapshot_report_sources(&self, plan: &crate::discover::Plan) -> Result<()> {
        let root = self.report_source_root();
        match fs::symlink_metadata(root.as_std_path()) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(error!("report source directory `{root}` is a link"));
            }
            Ok(metadata) if metadata.is_dir() => {
                fs::remove_dir_all(root.as_std_path())
                    .map_err(|cause| error!("could not clear report source directory `{root}`").caused_by(cause))?;
            }
            Ok(_) => return Err(error!("report source path `{root}` is not a directory")),
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => {}
            Err(cause) => {
                return Err(error!("could not inspect report source directory `{root}`").caused_by(cause));
            }
        }
        fs::create_dir(root.as_std_path()).map_err(|cause| error!("could not create report source directory `{root}`").caused_by(cause))?;

        for file in &plan.files {
            let Some(source) = file.source.as_ref() else { continue };
            let destination = root.join(&file.path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent.as_std_path())
                    .map_err(|cause| error!("could not create report source directory `{parent}`").caused_by(cause))?;
            }
            fs::write(destination.as_std_path(), source)
                .map_err(|cause| error!("could not retain report source `{destination}`").caused_by(cause))?;
        }
        Ok(())
    }

    /// The arguments every test binary is launched with.
    pub(super) fn test_arguments(&self) -> &[String] {
        &self.cargo.test_args
    }

    /// How this run's test binaries are executed, once the build has settled it.
    pub(super) const fn runner(&self) -> Option<&Harness> {
        self.nextest.as_ref()
    }

    /// The launch environment shared by every test binary this run starts.
    ///
    /// Derived once and then handed out, because both halves are invariant for the run and both sat
    /// on the hottest path the tool has. The stack floor reads the ambient variable exactly once
    /// here, which is also what keeps that read out of the per-launch path where other threads are
    /// concurrently spawning children.
    pub(super) fn launch(&self) -> &Launch {
        self.launch.get_or_init(|| Launch::derive(&self.libraries))
    }

    #[cfg(test)]
    pub(super) fn set_launch(&mut self, launch: Launch) {
        self.launch = OnceLock::from(launch);
    }

    #[cfg(test)]
    pub(super) fn set_harness_threads(&mut self, threads: &str) {
        self.harness_threads = OnceLock::from(Some(threads.to_owned()));
    }

    /// Settles how wide each spawned test harness runs, from the run's own job count.
    ///
    /// Called once, before anything is spawned, so that the baseline and every mutant after it are
    /// measured under the same contention. See [`Workspace::harness_threads`].
    pub(super) fn calibrate_harness(&self, jobs: usize) {
        self.calibrate_harness_with(jobs, harness_parallelism());
    }

    fn calibrate_harness_with(&self, jobs: usize, available: io::Result<NonZeroUsize>) {
        let cores =
        // #[gamma::skip(fallback.map_or_to_default, reason = "Only failed available_parallelism changes cores1->0. harness_threads returns None for explicit inherited threads, otherwise Some((cores/jobs.max(1)).max(1)); for every usize jobs, cores0 and cores1 both produce exactlySome1. Neither core fallback escapes elsewhere.", tag = "equivalent")]
        available.map_or(
        // #[gamma::skip(literal.int_decrement, reason = "Only failed available_parallelism changes cores1->0. harness_threads returns None for explicit inherited threads, otherwise Some((cores/jobs.max(1)).max(1)); for every usize jobs, cores0 and cores1 both produce exactlySome1. Neither core fallback escapes elsewhere.", tag = "equivalent")]
        1, NonZeroUsize::get);
        let inherited = inherited_harness_threads();

        let _settled = self
            .harness_threads
            .set(harness_threads(jobs, cores, inherited.as_deref()).map(|threads| threads.to_string()));
    }

    /// How many threads to tell a spawned test harness to use, or `None` to leave it alone.
    pub(super) fn harness_threads(&self) -> Option<&str> {
        self.harness_threads.get().and_then(Option::as_deref)
    }

    /// Reconstructs the environment Cargo would give the test executables from the metadata and
    /// successful build stream that produced them.
    pub(super) fn arm_test_environment(&mut self, binaries: &[TestBinary], artifacts: &str) -> Result<()> {
        self.prepend_test_search_paths(binaries);
        let metadata = self.capture_cargo_metadata()?;
        self.test_environment = Some(TestEnvironment::from_cargo(
            &metadata,
            artifacts,
            binaries,
            &self.root,
            self.cargo().get_program(),
        )?);
        self.cargo_metadata = Some(metadata);

        Ok(())
    }

    /// Adds the profile and dependency directories Cargo prepends when it runs a test binary.
    fn prepend_test_search_paths(&mut self, binaries: &[TestBinary]) {
        debug_assert!(
            self.launch.get().is_none(),
            "test search paths must be settled before deriving the launch environment"
        );
        let mut paths = Vec::new();

        for binary in binaries {
            let Some(directory) = binary.path.parent() else {
                continue;
            };
            if directory.file_name() == Some("deps")
                && let Some(profile) = directory.parent()
                && !paths.contains(&profile.to_owned())
            {
                paths.push(profile.to_owned());
            }
            if !paths.contains(&directory.to_owned()) {
                paths.push(directory.to_owned());
            }
        }

        for library in self.libraries.drain(..) {
            if library.file_name() == Some("deps")
                && let Some(profile) = library.parent()
                && !paths.contains(&profile.to_owned())
            {
                paths.push(profile.to_owned());
            }
            if !paths.contains(&library) {
                paths.push(library);
            }
        }
        self.libraries = paths;
    }

    /// Applies Cargo's shared and package-specific runtime variables.
    pub(super) fn configure_test_environment(&self, command: &mut Command, binary: &TestBinary) {
        if let Some(environment) = &self.test_environment {
            environment.configure(command, binary);
        }
    }

    #[cfg(test)]
    pub(super) fn set_test_environment(&mut self, package: &str, name: &str, value: impl Into<OsString>) {
        self.test_environment = Some(TestEnvironment::fake(package, name, value));
    }

    /// Hands the built tree to nextest, so every later mutant runs through it.
    ///
    /// Called after the build rather than at preparation, because the metadata it writes describes
    /// binaries that do not exist until then.
    ///
    /// # Errors
    ///
    /// Returns an error if nextest is not installed or does not recognise the built tree.
    pub(super) fn arm_nextest(&mut self, binaries: &[TestBinary]) -> Result<()> {
        let metadata = self
            .cargo_metadata
            .as_deref()
            .ok_or_else(|| error!("the Cargo test environment was not prepared before nextest"))?;
        self.nextest = Some(Harness::prepare(self, binaries, metadata)?);

        Ok(())
    }

    /// Installs a runner without asking nextest anything, for tests about what having one changes.
    ///
    /// Unused under `--cfg loom`, which compiles the ordinary test modules out and leaves only the
    /// concurrency models — so the sole caller of this helper disappears with them.
    #[cfg(test)]
    #[cfg_attr(loom, expect(dead_code, reason = "the loom build excludes the tests that call this"))]
    pub(super) fn set_runner(&mut self, harness: Harness) {
        self.nextest = Some(harness);
    }

    /// Writes a file this run generates for its own use, returning where it went.
    ///
    /// Kept beside the build artifacts rather than inside the copied tree, so that nothing a test
    /// walks over can see it and no mutant is judged against a tree that differs from the one that
    /// was built.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub(super) fn write_scratch(&self, name: &str, contents: &str) -> Result<Utf8PathBuf> {
        let path = self.target.join(name);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent.as_std_path()).map_err(|cause| error!("could not create `{parent}`").caused_by(cause))?;
        }

        fs::write(path.as_std_path(), contents).map_err(|cause| error!("could not write `{path}`").caused_by(cause))?;

        Ok(path)
    }

    /// Asks nextest to enumerate the test binaries this tree has already built.
    ///
    /// # Errors
    ///
    /// Returns an error if nextest is not installed or cannot read the tree.
    pub(super) fn capture_nextest_list(&self, binaries: &[TestBinary]) -> Result<String> {
        let command = self.nextest_list_command(binaries);

        self.capture(command, "cargo nextest list")
    }

    /// Builds the command that inventories the binaries nextest may run.
    fn nextest_list_command(&self, binaries: &[TestBinary]) -> Command {
        let mut command = self.cargo();
        let mut args = vec![
            "nextest".to_owned(),
            "list".to_owned(),
            "--list-type".to_owned(),
            "binaries-only".to_owned(),
            "--message-format".to_owned(),
            "json".to_owned(),
        ];

        let mut packages: Vec<&str> = binaries
            .iter()
            .map(|binary| {
                if binary.package_id.is_empty() {
                    binary.package.as_str()
                } else {
                    binary.package_id.as_str()
                }
            })
            .collect();
        packages.sort_unstable();
        packages.dedup();

        for package in packages {
            args.push("--package".to_owned());
            args.push(package.to_owned());
        }

        self.cargo.extend_nextest_args(&mut args);
        let _ = command.args(args);

        command
    }

    /// Describes the copied workspace to whoever needs to resolve its packages.
    ///
    /// # Errors
    ///
    /// Returns an error if cargo cannot read the tree.
    pub(super) fn capture_cargo_metadata(&self) -> Result<String> {
        let mut command = self.cargo();

        let _ = command.args(["metadata", "--format-version", "1"]);

        self.capture(command, "cargo metadata")
    }

    /// Runs a contained command that is expected to print something, and returns what it printed.
    ///
    /// Cargo metadata and nextest listing can execute build scripts or other repository-controlled
    /// helpers. Their stdout and stderr are drained concurrently, and every descendant is swept
    /// before inherited pipe handles are allowed to keep this capture open.
    fn capture(&self, command: Command, what: &str) -> Result<String> {
        #[cfg(all(test, not(miri)))]
        if let Some(output) = tests::scripted_capture(&command, what) {
            return interpret(output, what, &self.root);
        }

        interpret(
            contained_output(command, MemoryRequest::default()).map(Captured::from),
            what,
            &self.root,
        )
    }

    /// The campaign base containing this run's Cargo artifacts and reusable state.
    pub(super) fn base(&self) -> &Utf8Path {
        &self.base
    }
}

#[cfg(test)]
thread_local! {
    /// Fixture-owned current-thread parallelism and inherited-thread inputs; absent means native.
    static HARNESS_INPUTS: core::cell::Cell<Option<(usize, Option<&'static str>)>> = const { core::cell::Cell::new(None) };
}

fn harness_parallelism() -> io::Result<NonZeroUsize> {
    #[cfg(test)]
    if let Some((cores, _)) = HARNESS_INPUTS.get() {
        return NonZeroUsize::new(cores).ok_or_else(|| io::Error::other("the injected host has no parallelism estimate"));
    }
    thread::available_parallelism()
}

fn inherited_harness_threads() -> Option<String> {
    #[cfg(test)]
    if let Some((_, inherited)) = HARNESS_INPUTS.get() {
        return inherited.map(str::to_owned);
    }
    env::var(TEST_THREADS_VAR).ok()
}

fn rustc_capture_directory(root: &Utf8Path, target: &Utf8Path) -> Option<Utf8PathBuf> {
    wrapper_path()?;
    let cfg::RustcWrapperChain::Interpose(_) = cfg::rustc_wrapper_chain(root) else {
        return None;
    };

    Some(target.join(".cargo-gamma-rustc"))
}

fn wrapper_is_self(candidate: &OsStr, wrapper: &Utf8Path, root: &Utf8Path) -> bool {
    let candidate = std::path::Path::new(candidate);

    if candidate.components().count() == 1
        && candidate
            .file_stem()
            .and_then(OsStr::to_str)
            .zip(wrapper.file_stem())
            .is_some_and(|(candidate, wrapper)| executable_names_equal(candidate, wrapper))
    {
        return true;
    }

    let candidate = if
    // #[gamma::skip(cond.always_false, reason = "wrapper_is_self's absolute candidate.to_owned() is identical to root.join(candidate): Path::join replaces its base when the appended candidate is absolute. Therefore forcing the is_absolute guard false yields the same candidate before both canonicalizations, including absolute Windows drive and UNC paths.", tag = "equivalent")]
    candidate.is_absolute() {
        candidate.to_owned()
    } else {
        root.as_std_path().join(candidate)
    };
    fs::canonicalize(candidate)
        .ok()
        .zip(fs::canonicalize(wrapper.as_std_path()).ok())
        .is_some_and(|(candidate, wrapper)| candidate == wrapper)
}

fn executable_names_equal(left: &str, right: &str) -> bool {
    if
    // #[gamma::skip(cond.always_true, reason = "Target-specific equivalence for this archived Windows campaign: cfg!(windows) is the compile-time constant true. Replacing it with true preserves executable-name case folding. No claim of equivalence on Unix.", tag = "equivalent")]
    cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        // #[gamma::skip(relational.eq_to_ne, reason = "Target-specific equivalence for the archived Windows campaign: this equality is solely the else arm of if cfg!(windows), whose true arm uses eq_ignore_ascii_case. The mutated else arm is unreachable on Windows; Unix behavior would differ and is not claimed equivalent.", tag = "equivalent")]
        left == right
    }
}

/// What running a command produced, reduced to the three things [`interpret`] judges it on.
///
/// A [`std::process::Output`] carries an `ExitStatus`, which a test cannot construct without
/// dropping to a platform-specific `ExitStatusExt`. Narrowing to the success flag keeps the
/// judgement below constructible from a test on every platform this tool builds for.
#[derive(Debug)]
struct Captured {
    /// Whether the command reported success.
    succeeded: bool,

    /// What it printed on the standard output stream.
    stdout: Vec<u8>,

    /// What it printed on the standard error stream.
    stderr: Vec<u8>,
}

impl From<Output> for Captured {
    fn from(output: Output) -> Self {
        Self {
            succeeded: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

/// Judges a captured command, and returns what it printed.
///
/// Separated from the spawn because all three ways a capture can fail — the command not running at
/// all, running and reporting failure, and printing bytes that are not UTF-8 — are things the host
/// decides, and none can be arranged by running a real `cargo` against a real tree. Keeping the
/// judgement pure is what makes them reachable from a test, and it costs the production path
/// nothing: there is no seam to arm, no state to leak between threads, and nothing compiled in
/// that would not have been.
///
/// Every arm matters because the caller proceeds on this output as fact: a failure read as
/// empty-but-successful becomes a workspace with no test binaries and a run that cheerfully
/// reports nothing to do.
fn interpret<E>(captured: core::result::Result<Captured, E>, what: &str, root: &Utf8Path) -> Result<String>
where
    E: StdError + Send + Sync + 'static,
{
    let captured = captured.map_err(|cause| error!("could not run `{what}` in {root}").caused_by(cause))?;

    if !captured.succeeded {
        let stderr = String::from_utf8_lossy(&captured.stderr);

        return Err(error!("`{what}` failed in {root}:\n{}", stderr.trim()));
    }

    String::from_utf8(captured.stdout).map_err(|cause| error!("`{what}` did not print valid UTF-8").caused_by(cause))
}

/// Removes one scratch directory, treating one that is already gone as the outcome asked for.
///
/// A tree can be missing legitimately: a run that failed before the copy finished never made one,
/// and a caller may tear down after an error path has already cleared it. Neither is worth
/// reporting.
fn remove_tree(path: &Utf8Path) -> Result<()> {
    match fs::remove_dir_all(path.as_std_path()) {
        Err(cause) if cause.kind() != io::ErrorKind::NotFound => {
            Err(error!("could not remove the scratch directory at `{path}`").caused_by(cause))
        }
        _removed => Ok(()),
    }
}

/// Total size in bytes of everything a run keeps on disk under `base`.
///
/// Walking the tree costs a stat per file, and the tree includes the cargo target directory, which
/// on a large workspace holds hundreds of thousands of incremental artifacts. That is why this is a
/// function over a path rather than a figure every run computes: it is one line of output, so it is
/// worth only what it costs when something is actually going to print it.
///
/// Unreadable entries are skipped rather than failing a run that has already succeeded.
#[must_use]
pub fn footprint(base: &Utf8Path) -> u64 {
    WalkDir::new(base.as_std_path())
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .filter(fs::Metadata::is_file)
        .map(|metadata| metadata.len())
        .sum()
}

/// Where this tool keeps the synchronized source, vendored runtime, and stable workspace lock.
///
/// Outside the workspace by default, so Cargo cannot rediscover and merge the real workspace's
/// ancestor configuration after the copied configuration has already been rewritten.
///
/// The answer is always absolute. A relative `--cache-dir` is resolved against the current
/// directory, which is what a user typing one means, and absolutising here — before the synchronized
/// tree and vendored runtime are derived from it — makes both absolute by construction. It has to
/// be: the copy prunes its own destination by comparing it against the absolute paths it walks, and
/// cargo resolves a path dependency against the manifest holding it, so a relative base would make
/// the copy descend into its own output and the instrumented tree point its runtime dependency at
/// itself.
///
/// The default directory's name comes from `workspace_identity`, a digest this crate pins rather than
/// one the standard library is free to change: the lock every source-changing command shares lives
/// under this path, so two binaries that derive different names for one workspace would rewrite the
/// same tree at the same time while each believing it held the only lock.
#[must_use]
pub fn gamma_base(root: &Utf8Path, cache: Option<&Utf8Path>) -> Utf8PathBuf {
    let root = absolute(root);
    let base = cache.map_or_else(|| default_cache_home(&root).join(workspace_identity(&root)), Utf8Path::to_owned);

    absolute(&base)
}

/// Where this tool keeps Cargo artifacts and reusable campaign state.
///
/// An explicit cache keeps the historical all-in-one layout. Otherwise the state lives under
/// Cargo's resolved target directory, where `cargo clean` and ordinary disk-usage tools can find
/// the potentially very large build artifacts. The workspace identity keeps independently
/// configured workspaces separate when they deliberately share one target directory.
#[must_use]
pub fn campaign_base(root: &Utf8Path, target: &Utf8Path, cache: Option<&Utf8Path>) -> Utf8PathBuf {
    let root = absolute(root);
    let base = cache.map_or_else(|| default_campaign_base(&root, target), Utf8Path::to_owned);

    absolute(&base)
}

/// Resolves persisted campaign state, validating its workspace identity against Cargo's current
/// workspace topology.
#[must_use]
pub(crate) fn campaign_base_from_state(root: &Utf8Path, cache: Option<&Utf8Path>) -> Option<Utf8PathBuf> {
    campaign_state_from(root, cache).ok().map(|(_root, base)| base)
}

/// Resolves the workspace root and persisted campaign base for a state-consuming command.
pub(crate) fn campaign_state_from(selected: &Utf8Path, cache: Option<&Utf8Path>) -> Result<(Utf8PathBuf, Utf8PathBuf)> {
    campaign_state_from_with_metadata(selected, cache, |base| fs::symlink_metadata(base.as_std_path()))
}

fn campaign_state_from_with_metadata(
    selected: &Utf8Path,
    cache: Option<&Utf8Path>,
    inspect: impl FnOnce(&Utf8Path) -> io::Result<fs::Metadata>,
) -> Result<(Utf8PathBuf, Utf8PathBuf)> {
    let selected = physical(selected);

    if let Some(cache) = cache {
        let (root, _target) = metadata_workspace(&selected)
            .ok_or_else(|| error!("could not resolve the selected workspace before validating `--cache-dir`").usage())?;
        let base = gamma_base(&root, Some(cache));
        validate_campaign_state_owner(&root, &base)?;

        return Ok((root, base));
    }

    let metadata = metadata_workspace(&selected)
        .ok_or_else(|| error!("could not resolve the selected workspace before locating campaign state").usage())?;

    for ancestor in selected.ancestors() {
        let root = physical(ancestor);
        let locator = gamma_base(&root, None).join(CAMPAIGN_LOCATION);

        if let Ok(text) = fs::read_to_string(locator.as_std_path()) {
            let located = Utf8PathBuf::from(text.trim());
            if same_existing_path(&metadata.0, &root)
                && located.is_absolute()
                && cache_owner_root(&located).is_some_and(|owner| same_existing_path(&owner, &root))
            {
                return Ok((root, located));
            }
        }
    }

    let (root, target) = metadata;

    let base = campaign_base(&root, &target, None);
    match inspect(&base) {
        Ok(_) => validate_campaign_state_owner(&root, &base)?,
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => {}
        Err(cause) => {
            return Err(error!("could not inspect the campaign state at `{base}` before validating its owner").caused_by(cause));
        }
    }
    Ok((root, base))
}

fn validate_campaign_state_owner(root: &Utf8Path, base: &Utf8Path) -> Result<()> {
    let owner = cache_owner_root(base);
    if owner.as_ref().is_some_and(|owner| same_existing_path(owner, root)) {
        return Ok(());
    }

    let owner = owner.as_deref().map_or("<unowned>", Utf8Path::as_str);
    Err(error!("the cache at `{base}` is not owned by the selected workspace `{root}` (owner marker resolves to `{owner}`)").usage())
}

/// Resolves campaign state while holding the selected workspace's publication lock.
///
/// Resolution is repeated after the lock is acquired because a completing campaign publishes its
/// locator under that lock. The second resolution therefore observes either the completed
/// campaign's locator or the same state as the first resolution, never the stale locator that led
/// this command to the lock.
pub(crate) fn claim_campaign_state(selected: &Utf8Path, cache: Option<&Utf8Path>) -> Result<(Utf8PathBuf, Utf8PathBuf, File)> {
    claim_campaign_state_with(selected, cache, campaign_state_from)
}

fn claim_campaign_state_with(
    selected: &Utf8Path,
    cache: Option<&Utf8Path>,
    mut resolve: impl FnMut(&Utf8Path, Option<&Utf8Path>) -> Result<(Utf8PathBuf, Utf8PathBuf)>,
) -> Result<(Utf8PathBuf, Utf8PathBuf, File)> {
    let (root, _base) = resolve(selected, cache)?;
    let lock = claim_workspace(&root)?;
    let (locked_root, base) = resolve(selected, cache)?;

    if !same_existing_path(&root, &locked_root) {
        return Err(error!(
            "the selected workspace changed from `{root}` to `{locked_root}` while cargo-gamma was acquiring its state lock; retry the command"
        ));
    }

    Ok((root, base, lock))
}

pub(crate) fn claim_testing_state(selected: &Utf8Path, cache: Option<&Utf8Path>) -> Result<(Utf8PathBuf, Utf8PathBuf, File)> {
    claim_testing_state_with(selected, cache, locate_testing_state)
}

fn claim_testing_state_with(
    selected: &Utf8Path,
    cache: Option<&Utf8Path>,
    mut locate: impl FnMut(&Utf8Path, Option<&Utf8Path>) -> Result<(Utf8PathBuf, Utf8PathBuf)>,
) -> Result<(Utf8PathBuf, Utf8PathBuf, File)> {
    let (root, _) = locate(selected, cache)?;
    let lock = claim_workspace(&root)?;
    let (locked_root, base) = locate(selected, cache)?;
    if !same_existing_path(&root, &locked_root) {
        return Err(error!(
            "the testing campaign changed while acquiring its lock; retry `--resume-testing`"
        ));
    }
    Ok((root, base, lock))
}

fn locate_testing_state(selected: &Utf8Path, cache: Option<&Utf8Path>) -> Result<(Utf8PathBuf, Utf8PathBuf)> {
    let selected = physical(selected);
    if let Some(cache) = cache {
        let base = absolute(cache);
        let root = cache_owner_root(&base)
            .ok_or_else(|| error!("cannot resume testing: `{base}` has no valid campaign owner; start a new campaign"))?;
        if !selected_within_workspace(&selected, &root, same_existing_path) {
            return Err(error!(
                "cannot resume testing: `{base}` belongs to `{root}`, not the selected directory `{selected}`"
            ));
        }
        return Ok((root, base));
    }
    for ancestor in selected.ancestors() {
        let locator = gamma_base(ancestor, None).join(CAMPAIGN_LOCATION);
        let text = match fs::read_to_string(&locator) {
            Ok(text) => text,
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => continue,
            Err(cause) => return Err(error!("could not read testing campaign location `{locator}`").caused_by(cause)),
        };
        let base = Utf8PathBuf::from(text.trim());
        if !base.is_absolute() || !cache_owner_root(&base).is_some_and(|owner| same_existing_path(&owner, ancestor)) {
            return Err(error!(
                "testing campaign location `{locator}` has an invalid owner; start a new campaign"
            ));
        }
        return Ok((ancestor.to_owned(), base));
    }
    Err(error!(
        "cannot resume testing: no previous campaign under `{selected}` was interrupted during testing; start a new campaign"
    ))
}

fn selected_within_workspace(selected: &Utf8Path, root: &Utf8Path, mut same: impl FnMut(&Utf8Path, &Utf8Path) -> bool) -> bool {
    selected.ancestors().any(|ancestor| same(ancestor, root))
}

fn cache_owner_root(base: &Utf8Path) -> Option<Utf8PathBuf> {
    cache_owner_root_with_metadata(base, |path| fs::symlink_metadata(path.as_std_path()))
}

fn cache_owner_root_with_metadata(base: &Utf8Path, mut inspect: impl FnMut(&Utf8Path) -> io::Result<fs::Metadata>) -> Option<Utf8PathBuf> {
    let base_metadata = inspect(base).ok()?;
    if !base_metadata.is_dir() {
        return None;
    }

    let owner = base.join(CACHE_OWNER);
    let owner_metadata = inspect(&owner).ok()?;
    if !owner_metadata.is_file() || owner_metadata.len() > MAX_CACHE_OWNER_LEN {
        return None;
    }
    let text = fs::read_to_string(owner.as_std_path()).ok()?;
    let recorded = Utf8PathBuf::from(text);
    if !recorded.is_absolute() {
        return None;
    }
    let root = physical(&recorded);

    Some(root)
}

pub(crate) fn same_existing_path(left: &Utf8Path, right: &Utf8Path) -> bool {
    #[cfg(windows)]
    {
        let left = left.as_str().strip_prefix(r"\\?\").unwrap_or(left.as_str());
        let right = right.as_str().strip_prefix(r"\\?\").unwrap_or(right.as_str());

        left.eq_ignore_ascii_case(right)
    }

    // #[gamma::skip(bool_expr.negate, reason = "Target-specific equivalence for the archived Windows campaign: the exact mutated canonicalization match is immediately guarded by #[cfg(not(windows))], so it is removed before Windows compilation. Negating its result cannot affect this target's binary. No cross-platform equivalence is claimed.", tag = "equivalent")]
    #[cfg(not(windows))]
    match (
        fs::canonicalize(left.as_std_path()).ok(),
        fs::canonicalize(right.as_std_path()).ok(),
    ) {
        (Some(left), Some(right)) => left == right,
        _unresolved => false,
    }
}

fn metadata_workspace(selected: &Utf8Path) -> Option<(Utf8PathBuf, Utf8PathBuf)> {
    let mut command = cargo_metadata::MetadataCommand::new();
    let _command = command.current_dir(selected).no_deps();
    let metadata = command.exec().ok()?;

    Some((
        physical(Utf8Path::new(metadata.workspace_root.as_str())),
        physical(Utf8Path::new(metadata.target_directory.as_str())),
    ))
}

/// Remembers where the default campaign state for a workspace lives.
///
/// Returns whether the locator was published. Postprocessing advice depends on this: a campaign
/// record that exists but cannot be found by the advised command is not actionable.
pub(crate) fn remember_campaign_base(root: &Utf8Path, base: &Utf8Path) -> bool {
    let root = physical(root);
    if !cache_owner_root(base).is_some_and(|owner| same_existing_path(&owner, &root)) {
        return false;
    }

    let locator = gamma_base(&root, None).join(CAMPAIGN_LOCATION);
    match crate::elements::write(&locator, base.as_str()) {
        Ok(()) => true,
        Err(failure) => {
            crate::notes::note(format!("could not save campaign-state location: {failure}"));
            false
        }
    }
}

fn default_campaign_base(root: &Utf8Path, target: &Utf8Path) -> Utf8PathBuf {
    #[cfg(any(test, feature = "internals"))]
    if crate::testing::cache_home(root).is_some() {
        return gamma_base(root, None);
    }

    if env::var_os(TEST_CACHE_HOME_VAR).and_then(absolute_cache_home).is_some() {
        return gamma_base(root, None);
    }

    absolute(target).join("cargo-gamma/cache").join(workspace_identity(root))
}

/// The directory every default per-workspace cache is a child of.
fn default_cache_home(root: &Utf8Path) -> Utf8PathBuf {
    #[cfg(any(test, feature = "internals"))]
    if let Some(home) = crate::testing::cache_home(root) {
        return home.join("cargo-gamma");
    }

    if let Some(home) = env::var_os(TEST_CACHE_HOME_VAR).and_then(absolute_cache_home) {
        return home.join("cargo-gamma");
    }

    default_cache_home_with(root, |name| env::var_os(name))
}

/// The default base a normal process derives without any self-test override.
#[cfg(test)]
pub(crate) fn production_gamma_base(root: &Utf8Path) -> Utf8PathBuf {
    let root = absolute(root);
    absolute(&default_cache_home_with(&root, |name| env::var_os(name)).join(workspace_identity(&root)))
}

/// The target-resident campaign base a normal process derives without a self-test override.
pub(crate) fn production_campaign_base(root: &Utf8Path, target: &Utf8Path) -> Utf8PathBuf {
    let root = absolute(root);

    absolute(target).join("cargo-gamma/cache").join(workspace_identity(&root))
}

fn default_cache_home_with(root: &Utf8Path, mut get: impl FnMut(&str) -> Option<OsString>) -> Utf8PathBuf {
    get("XDG_CACHE_HOME")
        .and_then(absolute_cache_home)
        .or_else(|| get("LOCALAPPDATA").and_then(absolute_cache_home))
        .or_else(|| get("HOME").and_then(absolute_cache_home).map(|home| home.join(".cache")))
        .unwrap_or_else(|| absolute(root).parent().unwrap_or(root).join(".cargo-gamma-cache"))
        .join("cargo-gamma")
}

/// Accepts an environment-provided cache home only when every process resolves it identically.
///
/// Relative and empty environment values are ignored rather than resolved against the current
/// directory, because commands launched from different directories must still share one cache and
/// advisory lock for the same workspace.
fn absolute_cache_home(path: OsString) -> Option<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(path.into()).ok().filter(|path| path.is_absolute())
}

/// Names the default cache directory of one absolute workspace root.
///
/// BLAKE3 rather than `DefaultHasher`: the algorithm behind `DefaultHasher` is explicitly not a
/// stable contract across standard-library releases, and this name is not a private detail of one
/// process. It is where the advisory lock that serializes every command able to publish source or
/// configuration changes for this workspace lives, so two binaries built against different standard
/// libraries would take two different locks over one tree and copy or rewrite it concurrently while
/// each of them observed no contention. Pinning the algorithm here is what makes the documented
/// single lock domain true.
///
/// Shortened because this is a directory name a user reads and occasionally types. A collision is
/// caught rather than trusted: the owner marker records the root that owns the directory, and a
/// second root landing on the same name is refused instead of quietly sharing the tree.
// #[gamma::skip(all, reason = "the value comes from host identity or topology and cannot be replaced safely or deterministically in parallel tests")]
fn workspace_identity(root: &Utf8Path) -> String {
    let root = physical(root);
    digest_workspace_path(&root)
}

/// Digests the already-normalized spelling that defines one workspace's cache identity.
fn digest_workspace_path(root: &Utf8Path) -> String {
    let digest = blake3::hash(root.as_str().as_bytes());
    let mut identity = [u8::MIN; 8];

    identity.copy_from_slice(digest.as_bytes().get(..8).expect("a BLAKE3 digest is 32 bytes long"));

    format!("{:016x}", u64::from_be_bytes(identity))
}

/// Deletes every cached entry for a workspace while preserving its concurrency lock and identities.
///
/// Returns whether any cached data was removed. The lock file remains so an active run and a clean
/// cannot race over the workspace or Cargo artifacts, and the owner marker remains because it says
/// which workspace this directory is for rather than holding any cached data.
///
/// # Errors
///
/// Returns an error if the cache is unmarked or belongs to another workspace, if the cache
/// directory cannot be read, or if an entry cannot be removed — a permission denial, a file held
/// open by another process on Windows, or a path that changed under the walk.
///
/// Only the workspace's own cache is cleaned. A cache redirected elsewhere with `--cache-dir` is
/// not reachable from the workspace root alone and is the caller's to remove.
pub fn clean_cache(root: &Utf8Path, target: &Utf8Path) -> Result<bool> {
    clean_cache_with_guard(root, target, validate_default_cache_base)
}

fn clean_cache_with_guard(
    root: &Utf8Path,
    target: &Utf8Path,
    validate_scratch: impl FnOnce(&Utf8Path, &Utf8Path) -> Result<()>,
) -> Result<bool> {
    let scratch = gamma_base(root, None);
    let campaign = campaign_base(root, target, None);

    validate_scratch(root, &scratch)?;
    validate_default_campaign_base(root, target, &campaign)?;

    clean_cache_paths(root, &scratch, &campaign, || claim_workspace(root))
}

fn clean_cache_paths(root: &Utf8Path, scratch: &Utf8Path, campaign: &Utf8Path, claim: impl FnOnce() -> Result<File>) -> Result<bool> {
    let mut removed = false;
    if !scratch.exists() && !campaign.exists() {
        return Ok(false);
    }

    let _lock = claim()?;

    if scratch.exists() {
        removed |= clean_owned_cache(root, scratch, true)?;
    }

    if campaign.exists() && physical(campaign) != physical(scratch) {
        removed |= clean_owned_cache(root, campaign, false)?;
    }

    Ok(removed)
}

fn clean_owned_cache(root: &Utf8Path, base: &Utf8Path, preserve_lock: bool) -> Result<bool> {
    clean_owned_cache_with_entries(root, base, preserve_lock, |base| fs::read_dir(base.as_std_path()))
}

fn clean_owned_cache_with_entries<I>(
    root: &Utf8Path,
    base: &Utf8Path,
    preserve_lock: bool,
    list: impl FnOnce(&Utf8Path) -> io::Result<I>,
) -> Result<bool>
where
    I: IntoIterator<Item = io::Result<fs::DirEntry>>,
{
    reject_linked_cache(base)?;
    validate_cache_owner(root, base, CacheKind::Default)?;

    let entries = list(base).map_err(|cause| error!("could not read cargo-gamma's cache at `{base}`").caused_by(cause))?;
    let mut removed = false;

    for entry in entries {
        let entry = entry.map_err(|cause| error!("could not read an entry in cargo-gamma's cache at `{base}`").caused_by(cause))?;

        if entry.file_name() == CACHE_OWNER || preserve_lock && entry.file_name() == "lock" {
            continue;
        }

        remove_cached(&entry)?;
        removed = true;
    }

    Ok(removed)
}

/// Removes one entry of a cache directory, whatever kind of thing it is.
///
/// A symbolic link is unlinked rather than followed, so a link planted in a cache cannot turn a
/// clean into a recursive delete of whatever it points at.
fn remove_cached(entry: &fs::DirEntry) -> Result<()> {
    let path = entry.path();
    remove_cached_with_type(&path, || entry.file_type())
}

fn remove_cached_with_type(path: &std::path::Path, inspect: impl FnOnce() -> io::Result<fs::FileType>) -> Result<()> {
    let file_type = inspect().map_err(|cause| error!("could not inspect cached data at `{}`", path.display()).caused_by(cause))?;
    let result = if
    // #[gamma::skip(logical.and_remove_right, reason = "std::fs::FileType categories are exclusive: is_dir()/is_file() return false for symbolic links, including symlink_metadata results. The explicit is_symlink rejection beside an existing !is_dir/!is_file (or !is_symlink beside is_dir) is therefore logically redundant; removing only that operand preserves the predicate and effects.", tag = "equivalent")]
    file_type.is_dir() && !file_type.is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };

    result.map_err(|cause| error!("could not remove cached data at `{}`", path.display()).caused_by(cause))
}

/// Resolves a path against the current directory and removes the components that name nothing.
///
/// Purely textual, so it never touches the filesystem and never fails: the paths it is given are
/// scratch directories that do not exist yet, which is exactly when canonicalising cannot answer.
pub(crate) fn absolute(path: &Utf8Path) -> Utf8PathBuf {
    absolute_with_current_directory(path, env::current_dir)
}

fn absolute_with_current_directory(path: &Utf8Path, current_directory: impl FnOnce() -> io::Result<PathBuf>) -> Utf8PathBuf {
    // #[gamma::skip(all, reason = "the branch handles process, filesystem, platform, or synchronization state that cannot be forced safely and deterministically in unit tests")]
    let rooted = if path.is_absolute() {
        path.to_owned()
    } else {
        // A current directory that is not UTF-8, or that has been removed out from under the
        // process, leaves the path as written rather than failing a run over it.
        current_directory()
            .ok()
            .and_then(|cwd| Utf8PathBuf::from_path_buf(cwd).ok())
            .map_or_else(|| path.to_owned(), |cwd| cwd.join(path))
    };

    let mut normalised = Utf8PathBuf::new();

    for component in rooted.components() {
        match component {
            Utf8Component::CurDir => {}

            // `..` after a real directory name cancels it. After a root or another `..` there is
            // nothing to cancel, so it is kept and means what the filesystem says it means.
            Utf8Component::ParentDir => match normalised.components().next_back() {
                Some(Utf8Component::Normal(_)) => {
                    let _popped = normalised.pop();
                }
                _ => normalised.push(component),
            },

            other => normalised.push(other),
        }
    }

    normalised
}

/// Resolves the links in the part of `path` that exists, keeping the rest as written.
///
/// The scratch base usually does not exist yet, so the whole of it cannot be canonicalised. The
/// deepest ancestor that does exist is resolved and the remaining components are put back, which is
/// enough to answer the only question asked of it: where the path physically lands. A path that
/// cannot be resolved at all is returned as written, because being unable to answer must not fail
/// a run that would otherwise have been fine.
fn physical(path: &Utf8Path) -> Utf8PathBuf {
    #[cfg(any(test, feature = "internals"))]
    if let Some(resolved) = WORKSPACE_RESOLVER.with_borrow_mut(|provider| provider.as_mut().map(|resolve| resolve(path))) {
        return resolved;
    }
    let mut existing = path.to_owned();
    let mut tail: Vec<String> = Vec::new();

    loop {
        if let Ok(resolved) = fs::canonicalize(existing.as_std_path()) {
            let Ok(mut resolved) = Utf8PathBuf::from_path_buf(resolved) else {
                return path.to_owned();
            };

            for name in tail.iter().rev() {
                resolved.push(name);
            }

            return resolved;
        }

        let Some(name) = existing.file_name().map(str::to_owned) else {
            return path.to_owned();
        };

        tail.push(name);

        // #[gamma::skip(cond.always_false, tag = "timeout", reason = "the ancestor scan must stop when no further parent can be removed")]
        if !existing.pop() {
            return path.to_owned();
        }
    }
}

/// Refuses a scratch directory that the copy would end up copying into itself.
///
/// A base inside the workspace is normally fine, and is in fact the default: the copy is told to
/// skip it, so the tree it writes there is never walked. What cannot work is a base that lands
/// inside the tree in a place the copy's exclusion does not cover, and there are two ways to reach
/// that. One is a base that *is* the workspace root, because the root is the one path the walk does
/// not test against the exclusion — it is the tree being copied. The other is a base reached
/// through a link: the exclusion is a path comparison, so a base spelled as somewhere outside the
/// workspace never matches the paths the walk produces, even when it physically sits among them.
///
/// Both are decided on the physical paths, because that is what the copy walks, and refused up
/// front rather than allowed to proceed: the alternative is a tree that contains a copy of itself,
/// growing until the disk does, and every mutant measured in it measured in the wrong file.
fn ensure_copy_terminates(source: &Utf8Path, base: &Utf8Path) -> Result<()> {
    let (real_source, real_base) = (physical(source), physical(base));

    // `starts_with` is asked about the physical paths, because those are the ones the walk will
    // produce. Whether the copy really prunes is a separate question: the exclusion compares `base`
    // as written against paths the walk builds from `source` as written plus real directory
    // entries, so a `base` reached through a link is spelled one way and walked another and the
    // comparison never matches. Asking `prunes(source, base)` alone would take that as safe.
    if !real_base.starts_with(&real_source) || prunes_in_practice(source, base, &real_source, &real_base) {
        return Ok(());
    }

    if prunes(&real_source, &real_base) {
        return Err(error!(
            "the scratch directory `{base}` is `{real_base}`, inside the workspace at `{source}`, but is \
             not named as a path inside it — so the copy cannot skip it and would copy the copy.\n\
             Point --cache-dir at a directory that is really outside the workspace. To move only \
             compiler artifacts, set CARGO_TARGET_DIR or Cargo's build.target-dir instead."
        )
        .usage());
    }

    Err(error!(
        "the scratch directory `{base}` is the workspace at `{source}` itself, so copying the \
         workspace would copy the copy.\n\
         Point --cache-dir at a directory outside the workspace. To move only compiler artifacts, \
         set CARGO_TARGET_DIR or Cargo's build.target-dir instead."
    )
    .usage())
}

/// Ensures the synchronized workspace cannot be reached by a split-layout source walk.
///
/// An explicit all-in-one cache is the excluded base and can safely be inside the source. In the
/// default split layout only the campaign base is excluded, so the external scratch itself must be
/// physically outside the source even when a platform cache-home variable points inside it.
fn ensure_copy_layout_terminates(source: &Utf8Path, scratch: &Utf8Path, excluded: &Utf8Path) -> Result<()> {
    ensure_copy_terminates(source, excluded)?;

    let (real_source, real_scratch, real_excluded) = (physical(source), physical(scratch), physical(excluded));

    if real_scratch == real_excluded || !real_scratch.starts_with(&real_source) {
        return Ok(());
    }

    Err(error!(
        "cargo-gamma's external scratch directory `{scratch}` resolves to `{real_scratch}` inside \
         the workspace at `{source}`, but the split-layout copy excludes `{excluded}` instead.\n\
         Configure the platform cache home outside the workspace or pass --cache-dir to select a \
         safe all-in-one cache."
    )
    .usage())
}

/// Refuses a relocation that would hide VCS metadata a build can currently read.
///
/// The copier omits VCS directories deliberately. A default scratch tree lies below the source
/// tree and still reaches its ancestor metadata, while a relocated one may not. Letting the two
/// builds differ turns `--cache-dir` into an undocumented build-input switch, so the unsupported
/// arrangement is rejected before it creates or copies anything.
fn ensure_vcs_visibility(source: &Utf8Path, scratch: &Utf8Path) -> Result<()> {
    let source_metadata = visible_vcs_metadata(source);

    if
    // #[gamma::skip(cond.always_false, reason = "ensure_vcs_visibility's empty-source-metadata fast return may be removed: the following hidden vector iterates that same empty immutable vector and is necessarily empty regardless of scratch_metadata. The following hidden.is_empty return still returns Ok; visible_vcs_metadata only performs read-only discovery and returns a vector, not Result.", tag = "equivalent")]
    source_metadata.is_empty() {
        return Ok(());
    }

    let scratch_metadata = visible_vcs_metadata(scratch);
    let hidden: Vec<&Utf8PathBuf> = source_metadata.iter().filter(|marker| !scratch_metadata.contains(marker)).collect();

    if hidden.is_empty() {
        return Ok(());
    }

    Err(error!(
        "`--cache-dir` would relocate the cached workspace to `{scratch}`, where build scripts cannot see VCS metadata available from `{source}`: {}. \
         Use a cache directory beneath the same repository, or leave `--cache-dir` unset and set \
         `CARGO_TARGET_DIR` (or Cargo's `build.target-dir`) to move only compiler artifacts while \
         preserving the synchronized workspace's version-control context.",
        hidden.iter().map(|marker| marker.as_str()).collect::<Vec<_>>().join(", ")
    )
    .usage())
}

/// Makes source-control metadata visible from an isolated default scratch tree.
fn expose_vcs_metadata(source: &Utf8Path, scratch: &Utf8Path) -> Result<()> {
    expose_vcs_metadata_with(source, scratch, |marker, destination, directory| {
        super::copy::create_symlink(marker.as_std_path(), destination.as_std_path(), directory)
    })
}

fn expose_vcs_metadata_with(
    source: &Utf8Path,
    scratch: &Utf8Path,
    mut link: impl FnMut(&Utf8Path, &Utf8Path, bool) -> io::Result<()>,
) -> Result<()> {
    let markers = visible_vcs_metadata(source);

    for name in VCS_DIRS {
        let Some(marker) = markers
            .iter()
            .filter(|marker| marker.file_name() == Some(name))
            .max_by_key(|marker| marker.components().count())
        else {
            continue;
        };

        let destination = scratch.join(name);

        if name == ".git" {
            let git_dir = if marker.as_std_path().is_dir() {
                marker.clone()
            } else {
                let text = fs::read_to_string(marker.as_std_path())
                    .map_err(|cause| error!("could not read Git metadata pointer `{marker}`").caused_by(cause))?;
                let relative = text
                    .trim()
                    .strip_prefix("gitdir:")
                    .map(str::trim)
                    .ok_or_else(|| error!("Git metadata pointer `{marker}` has no `gitdir:` target"))?;

                absolute(
                    &marker
                        .parent()
                        .unwrap_or_else(|| {
                            Utf8Path::new(
                                // #[gamma::skip(literal.str_to_xyzzy, reason = "The fallback literal is read only if marker.parent() is None. This private expose_vcs_metadata marker is constructed by find_map over source.ancestors().map(|directory| directory.join(\".git\")); each successful marker necessarily ends in normal component .git and therefore has Some(parent), including a relative .git whose parent is the empty path. The fallback is unreachable regardless of the gitdir file's contents.", tag = "equivalent")]
                                "",
                            )
                        })
                        .join(relative),
                )
            };

            fs::write(destination.as_std_path(), format!("gitdir: {git_dir}\n"))
                .map_err(|cause| error!("could not expose Git metadata at `{destination}`").caused_by(cause))?;

            continue;
        }

        let linked = link(marker, &destination, marker.as_std_path().is_dir());
        linked.map_err(|cause| error!("could not expose VCS metadata at `{destination}`").caused_by(cause))?;
    }

    Ok(())
}

/// Returns whether the copy's exclusion will match a path the walk actually produces.
///
/// The walk descends from `source` as written and appends real directory entries, so the exclusion
/// fires only when `base` names those same components. That holds exactly when `base` is `source`
/// plus a relative tail and resolving that tail from the physical source lands on the physical base
/// — which is false as soon as any component of the tail is a symlink.
fn prunes_in_practice(source: &Utf8Path, base: &Utf8Path, real_source: &Utf8Path, real_base: &Utf8Path) -> bool {
    let Ok(relative) = base.strip_prefix(source) else {
        return false;
    };

    !relative.as_str().is_empty() && real_base == real_source.join(relative)
}

/// Returns whether the copy leaves `base` out of a walk of `source`.
///
/// The copy skips whole any path it walks that equals the scratch base, so a base strictly inside
/// the tree — and everything the run writes under it — never reaches the copy. The walk's own root
/// is never compared against it, which is the case this answers `false` for.
fn prunes(source: &Utf8Path, base: &Utf8Path) -> bool {
    base.starts_with(source) && base != source
}

/// Where the instrumented copy of a workspace lives.
///
/// Derived rather than stored so that a caller which was asked to keep the tree can say where it
/// is without the run having to hand back the workspace that owned it.
#[must_use]
pub fn scratch_tree(root: &Utf8Path, scratch: Option<&Utf8Path>) -> Utf8PathBuf {
    gamma_base(root, scratch).join("workspace")
}

/// Takes and validates a cache chosen independently of the workspace.
///
/// The caller has already taken the stable workspace lock. This second lock protects the inverse
/// collision: different workspaces naming the same redirected cache. Ownership is established only
/// for an empty directory; existing unmarked contents are never adopted as cargo-gamma state.
fn claim_redirected_cache(source: &Utf8Path, base: &Utf8Path) -> Result<File> {
    claim_redirected_cache_with(source, base, |owner| fs::symlink_metadata(owner.as_std_path()), has_any_entries)
}

fn claim_redirected_cache_with(
    source: &Utf8Path,
    base: &Utf8Path,
    inspect_owner: impl FnOnce(&Utf8Path) -> io::Result<fs::Metadata>,
    inventory: impl FnOnce(&Utf8Path) -> Result<bool>,
) -> Result<File> {
    if base.exists() {
        reject_linked_cache(base)?;

        match inspect_owner(&base.join(CACHE_OWNER)) {
            Ok(_metadata) => {}
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => {
                if inventory(base)? {
                    return Err(unowned_cache(base));
                }
            }
            Err(cause) => {
                return Err(error!(
                    "could not inspect the cargo-gamma cache owner marker at `{}`",
                    base.join(CACHE_OWNER)
                )
                .caused_by(cause));
            }
        }
    }

    create_private_dir_all(base)
        .map_err(|cause| error!("could not create the redirected cargo-gamma cache at `{base}`").caused_by(cause))?;

    reject_linked_cache(base)?;

    // #[gamma::skip(try.propagate_to_unwrap, reason = "On this Windows campaign target reject_foreign_writers is the infallible no-op adapter; Unix enforces native owner/permission checks that cannot be forced by deterministic host-independent input.", tag = "native-boundary")]
    reject_foreign_writers(base)?;

    let lock = claim(base)?;
    validate_cache_owner(source, base, CacheKind::Redirected)?;

    Ok(lock)
}

/// Creates a cache directory without granting access beyond the invoking user.
///
/// The process umask may be permissive on build agents and in containers. Requesting no group or
/// other permissions makes the result private under every umask; existing directories are left
/// unchanged and are still judged by [`reject_foreign_writers`].
#[cfg(unix)]
fn create_private_dir_all(path: &Utf8Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path.as_std_path())
}

#[cfg(not(unix))]
fn create_private_dir_all(path: &Utf8Path) -> io::Result<()> {
    fs::create_dir_all(path.as_std_path())
}

/// Refuses a cache directory that is reached through a link.
///
/// Everything under this directory is later handed to cargo as `CARGO_TARGET_DIR` and executed
/// from. A symbolic link — or, on Windows, any other reparse point — means the name that was
/// checked and the directory that is used are two different things, and whoever can rewrite the
/// link chooses the second one after the first has been approved. Only the final component is
/// checked here; the ancestry above an explicit cache is covered by [`reject_foreign_writers`] on
/// the platforms where ownership can be established at all.
fn reject_linked_cache(base: &Utf8Path) -> Result<()> {
    let metadata =
        inspect_cache_path(base).map_err(|cause| error!("could not inspect the cargo-gamma cache at `{base}`").caused_by(cause))?;

    if metadata.file_type().is_symlink() {
        return Err(error!(
            "the cargo-gamma cache at `{base}` is a link.\n\
             A cache link can be repointed after it has been checked, at a directory whose contents \
             this tool would then build and run."
        )
        .usage());
    }

    if !metadata.is_dir() {
        return Err(error!(
            "the cargo-gamma cache at `{base}` is not a directory.\n\
             Choose a directory for cargo-gamma's cache state."
        )
        .usage());
    }

    Ok(())
}

/// Replaces workspace-root resolution for a fixture on its installing thread.
#[cfg(any(test, feature = "internals"))]
type WorkspaceResolver = Box<dyn FnMut(&Utf8Path) -> Utf8PathBuf>;
/// Replaces non-following cache metadata queries, including scripted failures.
#[cfg(any(test, feature = "internals"))]
type CacheMetadataProvider = Box<dyn FnMut(&Utf8Path) -> io::Result<fs::Metadata>>;

#[cfg(any(test, feature = "internals"))]
thread_local! {
    static WORKSPACE_RESOLVER: std::cell::RefCell<Option<WorkspaceResolver>> = const { std::cell::RefCell::new(None) };
    static CACHE_METADATA: std::cell::RefCell<Option<CacheMetadataProvider>> = const { std::cell::RefCell::new(None) };
}

fn inspect_cache_path(path: &Utf8Path) -> io::Result<fs::Metadata> {
    #[cfg(any(test, feature = "internals"))]
    if let Some(metadata) = CACHE_METADATA.with_borrow_mut(|provider| provider.as_mut().map(|inspect| inspect(path))) {
        return metadata;
    }
    fs::symlink_metadata(path.as_std_path())
}

/// Refuses a cache whose directory, or any directory above it, another local user can write to.
///
/// Everything the run builds is placed here and later executed, so write access to any directory on
/// the path to it is enough to choose what this tool runs as the invoking user. The check walks the
/// physical path — the one with every link already resolved — from the cache to the root, and
/// refuses a directory that is owned by anyone but the invoking user or `root`. The cache itself
/// must not be writable by group or other. A shared ancestor is accepted only when its sticky bit
/// stops one user replacing another's entries.
///
/// This is an ownership check, not a race-free one. It says the directories are not writable by
/// another identity now, which is what makes a later substitution require the invoking user's own
/// credentials; it does not, and cannot, freeze the path for the duration of the run. A user who
/// can write to their own cache can still change it under a run they started themselves, and no
/// check here would be about a trust boundary.
#[cfg(unix)]
fn reject_foreign_writers(base: &Utf8Path) -> Result<()> {
    /// The bits that let a group member or anyone else write to a directory.
    const SHARED_WRITE: u32 = 0o022;

    /// The bit that stops one user removing or renaming another's entries in a shared directory.
    /// It is what makes a world-writable `/tmp` an acceptable place to keep a private subdirectory.
    const STICKY: u32 = 0o1000;

    /// The user id the kernel treats as unrestricted, and so the only foreign owner that could not
    /// gain anything by substituting a file this run will execute.
    const ROOT: u32 = 0;

    let user = cargo_gamma_unsafe::identity::effective_user();
    let physical = physical(base);

    for (index, directory) in physical.ancestors().enumerate() {
        // A relative cache path whose prefix could not be resolved ends its ancestry at the empty
        // path, which names no directory and is not a step towards the root.
        if directory.as_str().is_empty() {
            break;
        }

        let metadata = fs::symlink_metadata(directory.as_std_path())
            .map_err(|cause| error!("could not inspect `{directory}` on the way to the redirected cache").caused_by(cause))?;

        if metadata.uid() != user && metadata.uid() != ROOT {
            return Err(error!(
                "`{directory}`, on the way to the redirected cache at `{base}`, belongs to another user.\n\
                 Choose a directory only you can write to for --cache-dir: this run builds test executables \
                 there and then runs them as you."
            )
            .usage());
        }

        let mode = metadata.mode();

        if mode & SHARED_WRITE != 0 && (index == 0 || mode & STICKY == 0) {
            return Err(error!(
                "`{directory}`, on the way to the redirected cache at `{base}`, is writable by other users.\n\
                 Choose a directory only you can write to for --cache-dir: this run builds test executables \
                 there and then runs them as you."
            )
            .usage());
        }
    }

    Ok(())
}

/// Ownership of a cache directory cannot be established here.
///
/// The check this replaces asks who owns a directory and who may write to it. On Windows those are
/// answers about a discretionary access-control list, which `std` does not expose and which cannot
/// be read without the Win32 security interface; the closest thing to it that is reachable, the
/// read-only attribute, says nothing about other principals. Rather than run a check that would
/// pass on an unsafe directory and read as if something had been verified, nothing is claimed: a
/// redirected cache on this platform is trusted to the extent that the directory the user named is
/// one they control. The link check in [`reject_linked_cache`] still applies, and is the part that
/// does not depend on identity.
#[cfg(not(unix))]
#[expect(clippy::unnecessary_wraps, reason = "matches the Unix signature this stands in for")]
fn reject_foreign_writers(_base: &Utf8Path) -> Result<()> {
    Ok(())
}

/// Which cache directory a marker is being checked for, so the refusal names something the user
/// can change.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheKind {
    /// The per-workspace directory [`gamma_base`] derives when no `--cache-dir` was given.
    Default,

    /// A directory the user named with `--cache-dir`.
    Redirected,
}

impl CacheKind {
    /// What to tell a user whose cache directory already belongs to another workspace.
    const fn collision_advice(self) -> &'static str {
        match self {
            // The name is derived from the workspace root, so a mismatch is a digest collision
            // rather than a choice the user made — the only thing they can do is stop sharing it.
            Self::Default => "Pass --cache-dir to give one of the two workspaces a cache of its own.",
            Self::Redirected => "Choose a different directory for --cache-dir.",
        }
    }
}

/// Refuses a cache directory unless it is unclaimed or already belongs to this workspace.
///
/// The marker is opened once and both inspected and read through that one handle, so the file whose
/// type is checked is the file whose contents decide the answer. Reopening it by name between the
/// two would leave an interval in which the checked file could be replaced by another.
fn validate_cache_owner(source: &Utf8Path, base: &Utf8Path, kind: CacheKind) -> Result<()> {
    validate_cache_owner_with_metadata(source, base, kind, File::metadata)
}

fn validate_cache_owner_with_metadata(
    source: &Utf8Path,
    base: &Utf8Path,
    kind: CacheKind,
    inspect: impl FnOnce(&File) -> io::Result<fs::Metadata>,
) -> Result<()> {
    validate_cache_owner_with_inventory(source, base, kind, inspect, has_unowned_entries)
}

fn validate_cache_owner_with_inventory(
    source: &Utf8Path,
    base: &Utf8Path,
    kind: CacheKind,
    inspect: impl FnOnce(&File) -> io::Result<fs::Metadata>,
    inventory: impl FnOnce(&Utf8Path) -> Result<bool>,
) -> Result<()> {
    validate_cache_owner_with_writer(source, base, kind, inspect, inventory, write_cache_owner)
}

fn validate_cache_owner_with_writer(
    source: &Utf8Path,
    base: &Utf8Path,
    kind: CacheKind,
    inspect: impl FnOnce(&File) -> io::Result<fs::Metadata>,
    inventory: impl FnOnce(&Utf8Path) -> Result<bool>,
    write_owner: impl FnOnce(&mut File, &Utf8Path, &Utf8Path) -> Result<()>,
) -> Result<()> {
    let owner = base.join(CACHE_OWNER);
    let source = physical(source);

    match open_cache_owner(&owner) {
        Ok(mut marker) => {
            let metadata = inspect(&marker)
                .map_err(|cause| error!("could not inspect the cargo-gamma cache owner marker at `{owner}`").caused_by(cause))?;

            if !metadata.is_file() {
                return Err(error!(
                    "the cargo-gamma cache owner marker at `{owner}` is not a regular file.\n\
                     {}",
                    kind.collision_advice()
                )
                .usage());
            }

            if metadata.len() == 0 || metadata.len() > MAX_CACHE_OWNER_LEN {
                return Err(error!(
                    "the cargo-gamma cache owner marker at `{owner}` has an invalid length.\n\
                     {}",
                    kind.collision_advice()
                )
                .usage());
            }

            let mut recorded = String::new();

            (&mut marker)
                .take(MAX_CACHE_OWNER_LEN.saturating_add(1))
                .read_to_string(&mut recorded)
                .map_err(|cause| error!("could not read the cargo-gamma cache owner marker at `{owner}`").caused_by(cause))?;

            if u64::try_from(recorded.len()).unwrap_or(
                // #[gamma::skip(expr.decrement, reason = "recorded.len() is usize on supported 32/64-bit targets and always fits u64. The u64::MAX fallback is therefore unreachable.", tag = "equivalent")]
                u64::MAX,
            ) != metadata.len()
            {
                return Err(error!(
                    "the cargo-gamma cache owner marker at `{owner}` has an invalid length.\n\
                     {}",
                    kind.collision_advice()
                )
                .usage());
            }

            if recorded != source.as_str() {
                return Err(error!(
                    "the cargo-gamma cache at `{base}` belongs to the workspace at `{}`, not `{source}`.\n\
                     {}",
                    crate::report::encode_controls(&recorded),
                    kind.collision_advice()
                )
                .usage());
            }
        }
        Err(cause) if cause.kind() == io::ErrorKind::NotFound => {
            if inventory(base)? {
                return Err(unowned_cache(base));
            }

            // Created rather than truncated, so that this cannot adopt a marker another principal
            // planted between the check above and this write: a marker that appears in that window
            // makes the claim fail rather than silently overwriting whatever is there.
            let mut marker = File::options()
                .create_new(true)
                .write(true)
                .open(owner.as_std_path())
                .map_err(|cause| error!("could not write the cargo-gamma cache owner marker at `{owner}`").caused_by(cause))?;

            write_owner(&mut marker, &source, &owner)?;
        }
        Err(cause) => {
            return Err(error!("could not inspect the cargo-gamma cache owner marker at `{owner}`").caused_by(cause));
        }
    }

    Ok(())
}

fn write_cache_owner(marker: &mut impl Write, source: &Utf8Path, owner: &Utf8Path) -> Result<()> {
    marker
        .write_all(source.as_str().as_bytes())
        .map_err(|cause| error!("could not write the cargo-gamma cache owner marker at `{owner}`").caused_by(cause))?;
    Ok(())
}

#[cfg(unix)]
fn open_cache_owner(owner: &Utf8Path) -> io::Result<File> {
    // O_NOFOLLOW rejects a final-component symlink at open time. O_NONBLOCK prevents a planted
    // special file such as a FIFO from blocking before handle metadata can reject it.
    File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(owner.as_std_path())
}

#[cfg(not(unix))]
fn open_cache_owner(owner: &Utf8Path) -> io::Result<File> {
    let metadata = fs::symlink_metadata(owner.as_std_path())?;

    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache owner marker is not a regular file",
        ));
    }

    File::open(owner.as_std_path())
}

/// Says whether a directory already contains anything at all.
fn has_any_entries(base: &Utf8Path) -> Result<bool> {
    let mut entries =
        fs::read_dir(base.as_std_path()).map_err(|cause| error!("could not inspect the redirected cache at `{base}`").caused_by(cause))?;

    entries
        .next()
        .transpose()
        .map(|entry| entry.is_some())
        .map_err(|cause| error!("could not inspect an entry in the redirected cache at `{base}`").caused_by(cause))
}

/// Says whether a directory contains anything cargo-gamma has not just created to lock it.
fn has_unowned_entries(base: &Utf8Path) -> Result<bool> {
    let entries = fs::read_dir(base.as_std_path()).map_err(|cause| error!("could not inspect the cache at `{base}`").caused_by(cause))?;

    has_unowned_entries_from(base, entries)
}

fn has_unowned_entries_from(base: &Utf8Path, entries: impl IntoIterator<Item = io::Result<fs::DirEntry>>) -> Result<bool> {
    for entry in entries {
        let entry = entry.map_err(|cause| error!("could not inspect an entry in the cache at `{base}`").caused_by(cause))?;
        if entry.file_name() != "lock" {
            return Ok(true);
        }
    }

    Ok(false)
}

fn unowned_cache(base: &Utf8Path) -> crate::error::Error {
    error!(
        "the cache at `{base}` is not empty and is not marked as cargo-gamma state.\n\
         Existing contents will not be adopted or removed; pass --cache-dir to use an empty directory instead."
    )
    .usage()
}

/// Takes a cargo-gamma workspace or cache for the duration of an operation.
///
/// The lock is advisory and held by an open file, so it is released when the process ends however
/// it ends — there is no stale lock to clear after a crash.
fn claim(base: &Utf8Path) -> Result<File> {
    let path = base.join("lock");

    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.as_std_path())
        .map_err(|cause| error!("could not open the scratch lock at `{path}`").caused_by(cause))?;

    // A filesystem that cannot lock at all is not a filesystem another run is holding the lock on,
    // and the two call for opposite responses. Reported as contention, the user is told to wait for
    // a run that does not exist and to move the scratch directory — which cannot help, since the new
    // location is on the same mount unless they happen to guess otherwise.
    match try_lock(&file) {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(error!(
                "another `cargo gamma` run is already using `{base}`.\n\
                 Wait for it to finish before running another command that uses this workspace or cache."
            )
            .usage());
        }

        Err(TryLockError::Error(cause)) => {
            return Err(error!(
                "the cargo-gamma workspace lock at `{path}` could not be taken.\n\
                 The workspace cache must be on a filesystem that supports advisory file locking."
            )
            .caused_by(cause));
        }
    }

    Ok(file)
}

/// Takes the one stable lock shared by every command targeting `root`.
///
/// Deliberately ignores `--cache-dir`: separate build caches may run independently, but commands
/// that can publish source or configuration changes must still agree on one lock domain.
///
/// The cache owner marker is verified under the lock, so a second workspace root that happened to
/// derive the same directory name is refused rather than handed a lock domain and a scratch tree
/// belonging to the first. Checking it after the lock is taken is what makes the check meaningful:
/// before it, two colliding runs could both read an absent marker and both write one.
pub(crate) fn claim_workspace(root: &Utf8Path) -> Result<File> {
    let base = gamma_base(root, None);

    claim_workspace_at(root, &base)
}

/// Claims an already-derived default base after checking its production shape.
fn claim_workspace_at(root: &Utf8Path, base: &Utf8Path) -> Result<File> {
    validate_default_cache_base(root, base)?;

    fs::create_dir_all(base.as_std_path())
        .map_err(|cause| error!("could not create cargo-gamma's workspace cache at `{base}`").caused_by(cause))?;

    let lock = claim(base)?;

    validate_cache_owner(root, base, CacheKind::Default)?;

    Ok(lock)
}

/// Refuses a malformed default base before any directory, lock, marker, or cached entry is touched.
///
/// This is deliberately independent of [`gamma_base`]'s joins. Mutation testing changes one
/// expression at a time, so removing the namespace or identity join from the derivation leaves
/// this structural check intact and turns the mutant into a refusal rather than a write to the
/// platform cache home.
fn validate_default_cache_base(root: &Utf8Path, base: &Utf8Path) -> Result<()> {
    let root = absolute(root);
    let expected_identity = workspace_identity(&root);
    let shaped = base.file_name() == Some(expected_identity.as_str())
        && base.parent().is_some_and(|namespace| namespace.file_name() == Some("cargo-gamma"));

    if !shaped {
        return Err(error!(
            "cargo-gamma refused the malformed default cache path `{base}`: \
             a default cache must be an identity directory beneath the `cargo-gamma` cache namespace"
        ));
    }

    #[cfg(any(test, feature = "internals"))]
    if let Some(home) = crate::testing::cache_home(&root)
        && !base.starts_with(&home)
    {
        return Err(error!(
            "cargo-gamma's self-test cache path `{base}` escaped its private cache home `{home}`"
        ));
    }

    Ok(())
}

/// Refuses a malformed target-resident campaign base before it can be claimed or cleaned.
fn validate_default_campaign_base(root: &Utf8Path, target: &Utf8Path, base: &Utf8Path) -> Result<()> {
    let root = absolute(root);

    #[cfg(any(test, feature = "internals"))]
    if crate::testing::cache_home(&root).is_some() {
        let expected = gamma_base(&root, None);

        return if base == expected {
            Ok(())
        } else {
            Err(error!(
                "cargo-gamma's self-test campaign cache path `{base}` escaped its private cache base `{expected}`"
            ))
        };
    }

    if env::var_os(TEST_CACHE_HOME_VAR).and_then(absolute_cache_home).is_some() {
        let expected = gamma_base(&root, None);

        return if base == expected {
            Ok(())
        } else {
            Err(error!(
                "cargo-gamma's self-test campaign cache path `{base}` escaped its private cache base `{expected}`"
            ))
        };
    }

    validate_production_campaign_base(&root, target, base)
}

fn validate_production_campaign_base(root: &Utf8Path, target: &Utf8Path, base: &Utf8Path) -> Result<()> {
    let expected_identity = workspace_identity(root);
    let shaped = base.file_name() == Some(expected_identity.as_str())
        && base.parent().is_some_and(|cache| cache.file_name() == Some("cache"))
        && base
            .parent()
            .and_then(Utf8Path::parent)
            .is_some_and(|namespace| namespace.file_name() == Some("cargo-gamma"));

    if !shaped {
        return Err(error!(
            "cargo-gamma refused the malformed campaign cache path `{base}`: \
             a default campaign cache must be an identity directory beneath `cargo-gamma/cache`"
        ));
    }

    let expected = production_campaign_base(root, target);

    if base != expected {
        return Err(error!(
            "cargo-gamma refused the malformed campaign cache path `{base}`: \
             the resolved Cargo target requires `{expected}`"
        ));
    }

    Ok(())
}

fn claim_default_campaign_cache(root: &Utf8Path, target: &Utf8Path, base: &Utf8Path) -> Result<()> {
    validate_default_campaign_base(root, target, base)?;
    fs::create_dir_all(base.as_std_path())
        .map_err(|cause| error!("could not create cargo-gamma's campaign cache at `{base}`").caused_by(cause))?;
    reject_linked_cache(base)?;
    validate_cache_owner(root, base, CacheKind::Default)
}

/// Takes every lock needed to use one workspace's reusable state.
pub(crate) fn claim_cache(root: &Utf8Path, target: &Utf8Path, cache: Option<&Utf8Path>) -> Result<CacheLocks> {
    claim_cache_with_resolver(root, target, cache, physical)
}

fn claim_cache_with_resolver(
    root: &Utf8Path,
    target: &Utf8Path,
    cache: Option<&Utf8Path>,
    mut resolve: impl FnMut(&Utf8Path) -> Utf8PathBuf,
) -> Result<CacheLocks> {
    let workspace = claim_workspace(root)?;
    let base = campaign_base(root, target, cache);
    let scratch = gamma_base(root, None);
    let default = campaign_base(root, target, None);
    let redirected = if cache.is_some() && resolve(&base) == resolve(&scratch) {
        None
    } else if cache.is_some() && resolve(&base) != resolve(&default) {
        Some(claim_redirected_cache(root, &base)?)
    } else {
        claim_default_campaign_cache(root, target, &base)?;
        None
    };

    Ok(CacheLocks {
        workspace,
        redirected,
        #[cfg(any(test, feature = "internals"))]
        identity: next_cache_lock_identity(&NEXT_CACHE_LOCK_IDENTITY),
    })
}

/// The lock itself, named so the fault seam can stand in for a filesystem that cannot take one.
fn try_lock(file: &File) -> core::result::Result<(), TryLockError> {
    #[cfg(test)]
    if faults::fired(Fault::Lock) {
        return Err(TryLockError::Error(io::Error::from(io::ErrorKind::Unsupported)));
    }

    // Parallel tests can release and immediately reacquire one fixture's lock; tolerate that
    // in-process handoff without weakening production's fail-fast contention contract.
    #[cfg(test)]
    for _ in 0..250 {
        match file.try_lock() {
            Err(TryLockError::WouldBlock) => std::thread::sleep(std::time::Duration::from_millis(1)),
            result => return result,
        }
    }

    file.try_lock()
}

/// Repairs every manifest in the copied tree so that it still resolves from its new location.
///
/// Every manifest is visited, not just the ones belonging to mutated packages: a package nobody
/// mutates is still built, and a path dependency it cannot resolve fails the build just as surely.
fn anchor_manifests(source: &Utf8Path, root: &Utf8Path, runtime: &Utf8Path) -> Result<()> {
    let paths = WalkDir::new(root.as_std_path())
        .into_iter()
        .filter_map(core::result::Result::ok)
        .filter(|entry| entry.file_name() == "Cargo.toml")
        .map(walkdir::DirEntry::into_path);
    anchor_manifest_paths(source, root, runtime, paths)
}

fn anchor_manifest_paths(
    source: &Utf8Path,
    root: &Utf8Path,
    runtime: &Utf8Path,
    paths: impl IntoIterator<Item = std::path::PathBuf>,
) -> Result<()> {
    // Read before any manifest in the tree is edited: `root` is the copy of the whole workspace
    // `source` names, so its own `Cargo.toml` carries whatever `[workspace.dependencies]`
    // declares for the runtime crate — settings a member manifest that only says
    // `workspace = true` cannot see for itself once its own entry is replaced below. A missing,
    // unreadable, or malformed root manifest here just leaves nothing to inherit; the walk below
    // still reads and validates that same file in its own right, so a genuine problem with it is
    // reported from there instead of masked here.
    let workspace_features = WorkspaceRuntimeFeatures::from_workspace(&root.join("Cargo.toml")).unwrap_or_default();

    for path in paths {
        let Some(path) = Utf8Path::from_path(&path) else {
            continue;
        };

        let Some(original) = path.parent().and_then(|directory| directory.strip_prefix(root).ok()) else {
            continue;
        };

        let _destination = crate::paths::require_within(path, root, "a scratch manifest")?;
        let mut manifest = Manifest::read(path)?;

        manifest.anchor_paths(&source.join(original), original);
        manifest.redirect_runtime(runtime, &workspace_features)?;
        manifest.save()?;
    }

    anchor_cargo_config(root, source)?;

    // Done here rather than through `RUSTFLAGS`, which would replace the tree's own flags
    // instead of adding to them.
    cap_lints(root)
}

/// Extends whichever ambient rustflags setting cargo will read with [`CAP_LINTS`].
///
/// The instrumented tree is not the user's code, and lint levels configured for their code have no
/// authority over ours; denying warnings here would fail on any guarded expression a lint happens to
/// dislike. The flag is normally merged into the copied tree's `.cargo/config.toml` by `cap_lints`,
/// because setting a rustflags variable *replaces* the configured flags rather than adding to them.
/// An ambient setting outranks the configuration it stands for, though, so when the caller has one
/// the flag has to reach rustc through the variable instead.
///
/// Cargo reads flags from exactly one of four places, in this order: `CARGO_ENCODED_RUSTFLAGS`,
/// `RUSTFLAGS`, the target tables, then the build table. The first two are global and beat every
/// configured key, so when one of them is set it is the only thing extended. The last two are the
/// environment spellings of config keys `cap_lints` has already written to, and each *replaces* its
/// key rather than adding to it, so each present variable is extended — both levels, because which
/// of them cargo consults depends on a target triple that is not settled here, and every
/// `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` rather than the one naming the triple in force, for the same
/// reason `cap_lints` appends to every configured target table.
///
/// Missing any of this is not a warning: with `-D warnings` ambient the instrumented tree compiles
/// under deny-by-default, guard-induced warnings become errors, and the mutants that produced them
/// are withdrawn as unviable — a score computed over a silently smaller denominator.
fn cap_ambient_rustflags(command: &mut Command) {
    cap_ambient_rustflags_from(command, |name| env::var_os(name), env::vars_os);
}

fn cap_ambient_rustflags_from<I>(command: &mut Command, lookup: impl Fn(&str) -> Option<OsString>, variables: impl FnOnce() -> I)
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    // The encoded spelling separates flags with a unit separator rather than a space, which is the
    // whole reason it exists: a flag may contain spaces.
    if let Some(inherited) = lookup("CARGO_ENCODED_RUSTFLAGS") {
        return extend(command, "CARGO_ENCODED_RUSTFLAGS", inherited, "\u{1f}");
    }

    if let Some(inherited) = lookup("RUSTFLAGS") {
        return extend(command, "RUSTFLAGS", inherited, " ");
    }

    for (name, inherited) in variables() {
        let Some(name) = name.to_str() else {
            continue;
        };

        if name.starts_with("CARGO_TARGET_") && name.ends_with("_RUSTFLAGS") {
            extend(command, name, inherited, " ");
        }
    }

    if let Some(inherited) = lookup("CARGO_BUILD_RUSTFLAGS") {
        extend(command, "CARGO_BUILD_RUSTFLAGS", inherited, " ");
    }
}

/// Sets `name` on `command` to `inherited` with [`CAP_LINTS`] appended after `separator`.
fn extend(command: &mut Command, name: &str, inherited: OsString, separator: &str) {
    let mut merged = inherited;

    merged.push(separator);
    merged.push(CAP_LINTS);

    let _ = command.env(name, merged);
}

/// Returns the cargo executable to use, honouring the one that invoked us.
fn cargo_binary() -> String {
    env::var("CARGO").unwrap_or_else(|_missing| "cargo".to_owned())
}

/// Writes the guard runtime as a standalone crate outside the copied workspace.
///
/// Outside deliberately: a path dependency living inside a workspace directory but absent from its
/// member list makes cargo refuse to build, and editing the user's member list would be far more
/// invasive than dropping a crate next door.
fn vendor_runtime(at: &Utf8Path) -> Result<()> {
    let source = at.join("src");

    fs::create_dir_all(source.as_std_path()).map_err(|cause| error!("could not create `{source}`").caused_by(cause))?;

    let manifest = format!(
        "[package]\nname = \"{RUNTIME_PACKAGE}\"\nversion = \"0.0.0\"\nedition = \"{}\"\nrust-version = \"{}\"\npublish = false\n\n\
         [features]\nembedding = []\nloom = []\n\n[lints.rust]\nunexpected_cfgs = {{ level = \"warn\", check-cfg = ['cfg(coverage_nightly)', 'cfg(loom)'] }}\n\n\
         [lib]\nname = \"{RUNTIME_CRATE}\"\npath = \"src/lib.rs\"\n\n[workspace]\n",
        gamma_rt::embedded::EDITION,
        gamma_rt::embedded::RUST_VERSION
    );

    fs::write(at.join("Cargo.toml").as_std_path(), manifest)
        .map_err(|cause| error!("could not write the runtime manifest in `{at}`").caused_by(cause))?;

    for (name, contents) in gamma_rt::embedded::SOURCES {
        let path = source.join(name);

        fs::write(path.as_std_path(), contents).map_err(|cause| error!("could not write the runtime source `{path}`").caused_by(cause))?;
    }

    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod path_identity_tests {
    #[cfg(windows)]
    use super::same_existing_path;
    use super::{Utf8Path, selected_within_workspace};

    #[test]
    fn selection_uses_ancestor_identity_and_keeps_directory_boundaries() {
        let root = Utf8Path::new("workspace");
        for insensitive in [false, true] {
            for (directory, belongs) in [
                ("workspace", true),
                ("workspace/member", true),
                ("WORKSPACE/member", insensitive),
                ("workspace-other/member", false),
                ("elsewhere/member", false),
            ] {
                let actual = selected_within_workspace(Utf8Path::new(directory), root, |left, right| {
                    if insensitive {
                        left.as_str().eq_ignore_ascii_case(right.as_str())
                    } else {
                        left == right
                    }
                });
                assert_eq!(actual, belongs, "directory={directory}, insensitive={insensitive}");
            }
        }
    }

    #[test]
    #[cfg(windows)]
    fn windows_selection_accepts_casing_and_extended_prefix_changes() {
        let root = Utf8Path::new(r"C:\workspace");
        for directory in [r"c:\WORKSPACE", r"c:\WORKSPACE\member", r"\\?\C:\WORKSPACE\member"] {
            assert!(selected_within_workspace(Utf8Path::new(directory), root, same_existing_path));
        }
        assert!(!selected_within_workspace(
            Utf8Path::new(r"C:\workspace-other\member"),
            root,
            same_existing_path
        ));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod testing_resume_tests {
    use std::cell::RefCell;

    use super::super::testing_state::TestingJournal;
    use super::*;
    use crate::exec::verdict::Only;

    #[test]
    fn recovered_checkpoint_propagates_diagnostic_append_failures_after_successful_authoritative_records() {
        for mutant_checkpoint in [false, true] {
            let mut work = with_cache_home(|| Workspace::adopt("in-memory-checkpoint".into(), "never-written-checkpoint/target".into()));
            work.set_live_sweep_publisher_for_test(|_, _| Err(error!("named live-diagnostic refusal")));
            let (journal, entries) = TestingJournal::memory_failing_after_for_test(Some(1));
            work.set_journal_for_test(journal);
            let cost = super::super::session::SweepCost::default();
            let result = if mutant_checkpoint {
                work.checkpoint_mutant(&crate::fixtures::mutant(), &super::super::killers::Killers::default(), &cost)
            } else {
                work.checkpoint_sweep(&cost)
            };
            assert!(result.unwrap_err().to_string().contains("injected durable append error"));
            let entries = entries
                .lock()
                .expect("journal capture only appends or copies owned bytes under this lock")
                .clone();
            assert_eq!(entries.len(), 2);
            let diagnostic: serde_json::Value = serde_json::from_slice(&entries[1]).unwrap();
            assert!(
                diagnostic["diagnostic"]
                    .as_str()
                    .unwrap()
                    .starts_with("live sweep diagnostics refresh failed:")
            );
        }
    }

    #[test]
    fn recovered_adopted_workspace_keeps_runtime_capture_readiness_and_sorted_testing_roots() {
        let mut work = with_cache_home(|| Workspace::adopt("cache/workspace".into(), "cache/target".into()));
        assert_eq!(work.runtime, Utf8Path::new("cache/workspace/gamma-rt"));
        assert!(!work.rustc_captures_ready.load(Ordering::Relaxed));
        work.runtime = "cache/gamma-rt".into();
        assert_eq!(
            work.testing_roots().unwrap(),
            [Utf8PathBuf::from("cache/gamma-rt"), Utf8PathBuf::from("cache/workspace")]
        );
    }

    #[test]
    fn recovered_campaign_shape_rejections_name_the_exact_missing_namespace_component() {
        let root = Utf8Path::new(env!("CARGO_MANIFEST_DIR"));
        let target = root.join("uncreated-target");
        let identity = workspace_identity(root);
        for (namespace, cache, owner) in [
            ("cargo-gamma", "cache", "wrong-identity"),
            ("cargo-gamma", "wrong-cache", identity.as_str()),
            ("wrong-namespace", "cache", identity.as_str()),
        ] {
            let base = root.join(namespace).join(cache).join(owner);
            let failure = validate_production_campaign_base(root, &target, &base).unwrap_err().to_string();
            assert!(
                failure.contains("a default campaign cache must be an identity directory beneath `cargo-gamma/cache`"),
                "{failure}"
            );
            assert!(!failure.contains("the resolved Cargo target requires"), "{failure}");
        }
    }

    #[test]
    fn recovered_initial_hints_round_trip_every_identity_and_can_be_cleared_without_io() {
        let mut work = with_cache_home(|| Workspace::adopt("uncreated-workspace".into(), "uncreated-target".into()));
        work.testing_journal = Some(TestingJournal::memory_for_test(false).0);
        let hints = super::super::session::InitialHints {
            population: 7,
            killers: crate::HashMap::from_iter([(
                "mutant-id".into(),
                crate::discover::Killer {
                    package: "package".into(),
                    target: "test:target".into(),
                    test: "module::case".into(),
                },
            )]),
        };
        work.retain_initial_hints(Some(hints.clone()));
        let retained = work.initial_hints().unwrap();
        assert_eq!(retained.population, 7);
        assert_eq!(retained.killers, hints.killers);
        assert_eq!(work.testing_journal.as_ref().unwrap().initial_hints.as_ref().unwrap().population, 7);
        work.retain_initial_hints(None);
        assert!(work.initial_hints().is_none());
        assert!(work.testing_journal.as_ref().unwrap().initial_hints.is_none());
    }

    #[test]
    fn recovered_absent_journal_does_not_invent_a_live_refresh_failure() {
        let work = with_cache_home(|| Workspace::adopt("uncreated-workspace".into(), "uncreated-target".into()));
        let cost = super::super::session::SweepCost {
            accounting: Some(super::super::session::SweepAccounting::default()),
            ..super::super::session::SweepCost::default()
        };
        assert_eq!(
            work.with_sweep_diagnostics(Some(cost))
                .unwrap()
                .accounting
                .unwrap()
                .live_refresh_failures,
            0
        );
    }

    #[test]
    fn recovered_checkpoint_returns_a_journal_write_error_before_refreshing_diagnostics() {
        let mut work = with_cache_home(|| Workspace::adopt("in-memory-workspace".into(), "in-memory-target".into()));
        work.testing_journal = Some(TestingJournal::memory_for_test(true).0);
        work.set_live_sweep_publisher_for_test(|_, _| panic!("the authoritative journal failure must precede diagnostics"));
        let failure = work.checkpoint_sweep(&super::super::session::SweepCost::default()).unwrap_err();
        assert_eq!(failure.message(), "injected durable append error");
        assert_eq!(
            work.testing_journal.as_ref().unwrap().live_refresh_failures.load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn recovered_live_refresh_preserves_the_sidecar_name_and_reports_failed_diagnostic_publication() {
        let mut work = with_cache_home(|| Workspace::adopt("in-memory-workspace".into(), "in-memory-target".into()));
        work.base = "in-memory-campaign".into();
        work.set_live_sweep_publisher_for_test(|path, _| {
            assert_eq!(path, Utf8Path::new("in-memory-campaign").join("gamma-sweep-live.json"));
            Err(error!("named diagnostic publication refusal"))
        });
        let cost = super::super::session::SweepCost::default();
        let failure = work.write_live_sweep(&cost).unwrap_err();
        assert_eq!(failure.message(), "named diagnostic publication refusal");
        let journal = TestingJournal::memory_for_test(true).0;
        let failure = work.refresh_live_sweep(&journal, cost).unwrap_err();
        assert_eq!(failure.message(), "injected durable append error");
        assert_eq!(journal.live_refresh_failures.load(Ordering::Relaxed), 1);
    }

    fn with_cache_home<T>(operation: impl FnOnce() -> T) -> T {
        crate::testing::with_cache_home_at("controlled-cache-home".into(), operation)
    }

    fn workspace(base: &Utf8Path, scratch: &Utf8Path) -> Workspace {
        let mut original = with_cache_home(|| Workspace::adopt(scratch.join("workspace"), base.join("target")));
        original.runtime = scratch.join("rt");
        original.libraries = vec![scratch.join("libraries")];
        original.cargo.profile = Some("gamma".to_owned());
        original.cargo.test_args = vec!["--skip".into(), "slow".into()];
        original.test_environment = Some(TestEnvironment::fake("subject", "BUILD_SCRIPT_VALUE", "captured"));
        original.harness_threads = OnceLock::from(Some("3".into()));
        original.launch = OnceLock::from(Launch {
            loader: Some("captured-library-path".into()),
            stack: "7654321".into(),
        });
        original
    }

    fn snapshot(base: &Utf8Path, scratch: &Utf8Path) -> serde_json::Value {
        workspace(base, scratch).testing_snapshot().unwrap()
    }

    #[test]
    fn testing_resume_restores_launch_environment_arguments_and_idle_resource_admission() {
        let source = Utf8Path::new("source");
        let base = Utf8Path::new("cache");
        let default_scratch = Utf8Path::new("default-scratch");
        let mut original = workspace(base, base);
        let binary = TestBinary {
            package_id: "subject".into(),
            ..crate::testing::test_binary("a-test")
        };
        original.resources = Resources::fake_tests(&binary, &[("one", "database")]);
        let _held = original.acquire_resources(&binary, Only::One("one"));
        let encoded = original.testing_snapshot().unwrap();
        assert!(encoded["resources"].get("active").is_none());
        let calls = RefCell::new(Vec::new());
        let restored = with_cache_home(|| {
            Workspace::resume_testing(
                source,
                base,
                encoded.clone(),
                None,
                73,
                TestingResumeIo {
                    default_scratch: |actual_source: &Utf8Path, cache: Option<&Utf8Path>| {
                        assert_eq!(actual_source, source);
                        assert_eq!(cache, None);
                        calls.borrow_mut().push("scratch");
                        default_scratch.to_owned()
                    },
                    claim_cache: |actual_source: &Utf8Path, actual_base: &Utf8Path| {
                        assert_eq!((actual_source, actual_base), (source, base));
                        calls.borrow_mut().push("cache");
                        Ok(None)
                    },
                    open_journal: |actual_base: &Utf8Path, valid_bytes| {
                        assert_eq!(actual_base, base);
                        assert_eq!(valid_bytes, Some(73));
                        calls.borrow_mut().push("journal");
                        Ok(None)
                    },
                },
            )
        })
        .unwrap();
        assert_eq!(calls.into_inner(), ["scratch", "cache", "journal"]);
        assert!(restored.testing_journal.is_none());
        assert_eq!(restored.testing_snapshot().unwrap(), encoded);
        assert_eq!(restored.reporting_profile(), Some("gamma"));
        assert_eq!(restored.test_arguments(), ["--skip", "slow"]);
        assert_eq!(restored.harness_threads(), Some("3"));
        assert_eq!(restored.launch().stack, "7654321");
        assert_eq!(restored.launch().loader.as_deref(), Some(OsStr::new("captured-library-path")));
        let required = restored.required_resources(&binary, Only::One("one"));
        assert_eq!(required.iter().map(AsRef::as_ref).collect::<Vec<&str>>(), ["database"]);
        let mut command = Command::new("unused");
        restored.configure_test_environment(&mut command, &binary);
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            [(OsStr::new("BUILD_SCRIPT_VALUE"), Some(OsStr::new("captured")))]
        );
        let _fresh = restored.acquire_resources(&binary, Only::One("one"));
    }

    #[test]
    fn testing_resume_rejects_other_scheduler_versions_before_io() {
        for version in [None, Some(0), Some(1), Some(3)] {
            let mut encoded = snapshot(Utf8Path::new("cache"), Utf8Path::new("cache"));
            if let Some(version) = version {
                encoded["scheduler_version"] = version.into();
            } else {
                encoded.as_object_mut().unwrap().remove("scheduler_version");
            }
            let failure = TestingSnapshot::resume_with::<(), ()>(
                encoded,
                Utf8Path::new("source"),
                Utf8Path::new("cache"),
                || panic!("scheduler validation must precede scratch resolution"),
                |_, _| panic!("scheduler validation must precede cache admission"),
                |_| panic!("scheduler validation must precede journal recovery"),
            )
            .unwrap_err();
            assert!(failure.to_string().contains("start a new campaign without --resume-testing"));
        }
    }

    #[test]
    fn testing_resume_default_scratch_does_not_claim_a_redirected_cache() {
        let default_scratch = Utf8Path::new("default-scratch");
        for base in [Utf8Path::new("cache"), default_scratch] {
            let encoded = snapshot(base, default_scratch);
            let calls = RefCell::new(Vec::new());
            let restored = with_cache_home(|| {
                Workspace::resume_testing(
                    Utf8Path::new("source"),
                    base,
                    encoded.clone(),
                    None,
                    0,
                    TestingResumeIo {
                        default_scratch: |actual_source: &Utf8Path, cache: Option<&Utf8Path>| {
                            assert_eq!(actual_source, Utf8Path::new("source"));
                            assert_eq!(cache, None);
                            calls.borrow_mut().push("scratch");
                            default_scratch.to_owned()
                        },
                        claim_cache: |_: &Utf8Path, _: &Utf8Path| -> Result<Option<File>> {
                            panic!("default scratch must not claim a redirected cache")
                        },
                        open_journal: |actual_base: &Utf8Path, valid_bytes| {
                            assert_eq!(actual_base, base);
                            assert_eq!(valid_bytes, Some(0));
                            calls.borrow_mut().push("journal");
                            Ok(None)
                        },
                    },
                )
            })
            .unwrap();
            assert_eq!(calls.into_inner(), ["scratch", "journal"]);
            assert_eq!(restored.testing_snapshot().unwrap(), encoded);
        }
    }

    #[test]
    fn testing_resume_rejects_cross_workspace_paths_before_io() {
        let base = Utf8Path::new("cache");
        for (field, value) in [
            ("base", "other-cache"),
            ("target", "other-cache/target"),
            ("root", "other-scratch/workspace"),
            ("root", "cache/not-workspace"),
            ("runtime", "other-scratch/rt"),
        ] {
            let mut encoded = snapshot(base, base);
            encoded[field] = value.into();
            let failure = Workspace::resume_testing(
                Utf8Path::new("source"),
                base,
                encoded,
                None,
                19,
                TestingResumeIo {
                    default_scratch: |_: &Utf8Path, _: Option<&Utf8Path>| "default-scratch".into(),
                    claim_cache: |_: &Utf8Path, _: &Utf8Path| -> Result<Option<File>> { panic!("invalid paths must not claim a cache") },
                    open_journal: |_: &Utf8Path, _: Option<u64>| -> Result<Option<TestingJournal>> {
                        panic!("invalid paths must not open a journal")
                    },
                },
            )
            .unwrap_err();
            assert_eq!(
                failure.to_string(),
                "prepared testing workspace does not belong to `source` and `cache`; start a new campaign"
            );
            assert!(failure.source().is_none());
        }
    }

    #[test]
    fn recovered_testing_resume_rejects_a_consistent_foreign_scratch_pair_before_claiming_or_opening_anything() {
        let base = Utf8Path::new("cache");
        let foreign = Utf8Path::new("foreign-scratch");
        let encoded = snapshot(base, foreign);
        let failure = Workspace::resume_testing(
            Utf8Path::new("source"),
            base,
            encoded,
            None,
            19,
            TestingResumeIo {
                default_scratch: |_: &Utf8Path, _: Option<&Utf8Path>| "default-scratch".into(),
                claim_cache: |_: &Utf8Path, _: &Utf8Path| -> Result<Option<File>> {
                    panic!("a foreign scratch pair must not claim a cache");
                },
                open_journal: |_: &Utf8Path, _: Option<u64>| -> Result<Option<TestingJournal>> {
                    panic!("a foreign scratch pair must not open a journal");
                },
            },
        )
        .unwrap_err();
        assert_eq!(
            failure.message(),
            "prepared testing workspace does not belong to `source` and `cache`; start a new campaign"
        );
        assert!(failure.source().is_none());
    }

    #[test]
    fn testing_resume_rejects_malformed_state_before_io() {
        let failure = Workspace::resume_testing(
            Utf8Path::new("source"),
            Utf8Path::new("cache"),
            serde_json::json!({}),
            None,
            19,
            TestingResumeIo {
                default_scratch: |_: &Utf8Path, _: Option<&Utf8Path>| panic!("malformed state must not resolve default scratch"),
                claim_cache: |_: &Utf8Path, _: &Utf8Path| -> Result<Option<File>> { panic!("malformed state must not claim a cache") },
                open_journal: |_: &Utf8Path, _: Option<u64>| -> Result<Option<TestingJournal>> {
                    panic!("malformed state must not open a journal")
                },
            },
        )
        .unwrap_err();
        let cause = failure.source().unwrap().downcast_ref::<serde_json::Error>().unwrap();
        assert_eq!(cause.classify(), serde_json::error::Category::Data);
        assert_eq!(cause.to_string(), "missing field `base`");
    }

    #[test]
    fn testing_resume_rejects_missing_scratch_before_io() {
        let base = Utf8Path::new("cache");
        let mut encoded = snapshot(base, base);
        encoded["root"] = "/".into();
        let failure = Workspace::resume_testing(
            Utf8Path::new("source"),
            base,
            encoded,
            None,
            19,
            TestingResumeIo {
                default_scratch: |_: &Utf8Path, _: Option<&Utf8Path>| panic!("missing scratch must not resolve default scratch"),
                claim_cache: |_: &Utf8Path, _: &Utf8Path| -> Result<Option<File>> { panic!("missing scratch must not claim a cache") },
                open_journal: |_: &Utf8Path, _: Option<u64>| -> Result<Option<TestingJournal>> {
                    panic!("missing scratch must not open a journal")
                },
            },
        )
        .unwrap_err();
        assert_eq!(failure.to_string(), "prepared testing workspace has no scratch directory");
        assert!(failure.source().is_none());
    }

    #[test]
    fn testing_resume_cache_rejection_prevents_journal_recovery() {
        let base = Utf8Path::new("cache");
        let failure = Workspace::resume_testing(
            Utf8Path::new("source"),
            base,
            snapshot(base, base),
            None,
            19,
            TestingResumeIo {
                default_scratch: |_: &Utf8Path, _: Option<&Utf8Path>| "default-scratch".into(),
                claim_cache: |source: &Utf8Path, actual_base: &Utf8Path| -> Result<Option<File>> {
                    assert_eq!((source, actual_base), (Utf8Path::new("source"), base));
                    Err(error!("cache admission failed").caused_by(io::Error::new(io::ErrorKind::PermissionDenied, "foreign writer")))
                },
                open_journal: |_: &Utf8Path, _: Option<u64>| -> Result<Option<TestingJournal>> {
                    panic!("rejected cache must not open a journal")
                },
            },
        )
        .unwrap_err();
        let cause = failure.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(cause.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(cause.to_string(), "foreign writer");
        assert_eq!(failure.to_string(), "cache admission failed: foreign writer");
    }

    #[test]
    fn testing_resume_propagates_journal_failure_after_cache_admission() {
        #[derive(Debug)]
        struct Lock<'a>(&'a RefCell<Vec<&'static str>>, &'static str);

        impl Drop for Lock<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().push(self.1);
            }
        }

        impl From<Lock<'_>> for Option<File> {
            fn from(lock: Lock<'_>) -> Self {
                lock.0.borrow_mut().push("lock-transfer");
                None
            }
        }

        let base = Utf8Path::new("cache");
        let calls = RefCell::new(Vec::new());
        let failure = Workspace::resume_testing(
            Utf8Path::new("source"),
            base,
            snapshot(base, base),
            Lock(&calls, "workspace-drop"),
            19,
            TestingResumeIo {
                default_scratch: |_: &Utf8Path, _: Option<&Utf8Path>| "default-scratch".into(),
                claim_cache: |source: &Utf8Path, actual_base: &Utf8Path| {
                    assert_eq!((source, actual_base), (Utf8Path::new("source"), base));
                    calls.borrow_mut().push("cache");
                    Ok(Lock(&calls, "cache-drop"))
                },
                open_journal: |actual_base: &Utf8Path, valid_bytes| -> Result<Option<TestingJournal>> {
                    assert_eq!(actual_base, base);
                    assert_eq!(valid_bytes, Some(19));
                    calls.borrow_mut().push("journal");
                    Err(error!("journal recovery failed").caused_by(io::Error::new(io::ErrorKind::InvalidData, "truncated verdict")))
                },
            },
        )
        .unwrap_err();
        assert_eq!(calls.into_inner(), ["cache", "journal", "cache-drop", "workspace-drop"]);
        let cause = failure.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(cause.kind(), io::ErrorKind::InvalidData);
        assert_eq!(cause.to_string(), "truncated verdict");
        assert_eq!(failure.to_string(), "journal recovery failed: truncated verdict");
    }
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeMap;
    use std::error::Error as _;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt as _;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    #[cfg(windows)]
    use std::os::windows::ffi::OsStringExt as _;
    #[cfg(windows)]
    use std::os::windows::fs::OpenOptionsExt as _;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_dir;
    use std::path::Path;

    use serde_json::{Value, from_slice, from_str, from_value, json, to_string, to_value};

    use super::*;
    use crate::HashMap;
    use crate::testing::private_system_tempdir;

    #[test]
    fn recovered_cache_owner_writer_keeps_the_exact_source_bytes_and_returns_writer_errors() {
        struct RefusingWriter;
        impl Write for RefusingWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "named cache-owner write refusal"))
            }

            fn flush(&mut self) -> io::Result<()> {
                panic!("a failed owner write must not flush");
            }
        }
        let source = Utf8Path::new("never-written-source-identity");
        let owner = Utf8Path::new("never-created-owner-marker");
        let mut bytes = Vec::new();
        write_cache_owner(&mut bytes, source, owner).unwrap();
        assert_eq!(bytes, source.as_str().as_bytes());
        let failure = write_cache_owner(&mut RefusingWriter, source, owner).unwrap_err();
        assert_eq!(
            failure.message(),
            "could not write the cargo-gamma cache owner marker at `never-created-owner-marker`"
        );
        assert_eq!(
            failure.source().unwrap().downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn recovered_ambient_target_flags_after_a_non_unicode_variable_are_still_forwarded() {
        #[cfg(windows)]
        let invalid = OsString::from_wide(&[0xd800]);
        #[cfg(unix)]
        let invalid = OsString::from_vec(vec![0xff]);
        let mut command = Command::new("never-launched-rustflags-observer");
        cap_ambient_rustflags_from(
            &mut command,
            |_| None,
            || {
                [
                    (invalid, OsString::from("ignored")),
                    ("CARGO_TARGET_DISTINCT_RUSTFLAGS".into(), "-D warnings".into()),
                ]
            },
        );
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            [(
                OsStr::new("CARGO_TARGET_DISTINCT_RUSTFLAGS"),
                Some(OsStr::new(&format!("-D warnings {CAP_LINTS}")))
            )]
        );
    }

    #[test]
    fn recovered_native_harness_calibration_uses_the_exact_job_count_and_settles_its_cache_only_once() {
        struct Restore(Option<(usize, Option<&'static str>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                HARNESS_INPUTS.set(self.0);
            }
        }
        let _restore = Restore(HARNESS_INPUTS.replace(Some((8, None))));
        for (jobs, expected) in [(2, "4"), (3, "2"), (8, "1"), (16, "1")] {
            for native in [false, true] {
                let work = Workspace::adopt("never-read-harness-root".into(), "never-written-harness-target".into());
                assert!(work.harness_threads.get().is_none());
                if native {
                    work.calibrate_harness(jobs);
                } else {
                    work.calibrate_harness_with(jobs, Ok(NonZeroUsize::new(8).unwrap()));
                }
                assert_eq!(work.harness_threads.get(), Some(&Some(expected.to_owned())));
                assert_eq!(work.harness_threads(), Some(expected));
                work.calibrate_harness(1);
                assert_eq!(work.harness_threads(), Some(expected));
            }
        }
        HARNESS_INPUTS.set(Some((8, Some("3"))));
        let work = Workspace::adopt("never-read-inherited-root".into(), "never-written-inherited-target".into());
        work.calibrate_harness(2);
        assert_eq!(work.harness_threads.get(), Some(&None));
        assert_eq!(work.harness_threads(), None);
    }

    // Direct relaxed flag accesses below inspect or configure workspaces on the owning thread.
    // They publish no state to other threads and are not concurrent readiness checks.

    #[test]
    fn telemetry_live_refresh_failures_publish_evidence_and_propagate_failed_evidence() {
        use crate::exec::{SweepAccounting, SweepCost};

        let failures = AtomicUsize::new(0);
        let cost = SweepCost {
            accounting: Some(SweepAccounting::default()),
            ..SweepCost::default()
        };
        let mut evidence = None;
        Workspace::record_live_refresh(
            &failures,
            cost,
            Err(error!("could not refresh diagnostics").caused_by(io::Error::from(io::ErrorKind::PermissionDenied))),
            |cost, detail| {
                evidence = Some((cost, detail));
                Ok(())
            },
        )
        .unwrap();
        let (cost, detail) = evidence.unwrap();
        assert_eq!(failures.load(Ordering::Relaxed), 1);
        assert_eq!(cost.accounting.as_ref().unwrap().live_refresh_failures, 1);
        assert!(detail.contains("live sweep diagnostics refresh failed"));
        assert!(detail.contains("could not refresh diagnostics"));
        assert!(detail.contains(&io::Error::from(io::ErrorKind::PermissionDenied).to_string()));
        Workspace::record_live_refresh(&failures, cost.clone(), Ok(()), |_, _| {
            panic!("successful refreshes produce no failure evidence")
        })
        .unwrap();
        assert_eq!(failures.load(Ordering::Relaxed), 1);
        let failure = Workspace::record_live_refresh(&failures, cost, Err(error!("second refresh failed")), |updated, detail| {
            assert_eq!(updated.accounting.unwrap().live_refresh_failures, 2);
            assert!(detail.contains("second refresh failed"));
            Err(error!("durable evidence unavailable"))
        })
        .unwrap_err();
        assert_eq!(
            failure.to_string(),
            "durable evidence unavailable",
            "an unavailable journal is fatal, never silently ignored"
        );
        assert_eq!(failures.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn telemetry_live_refresh_failure_overlay_is_segment_local_across_snapshots_and_resume() {
        use crate::exec::{SweepAccounting, SweepCost};

        let prior = SweepCost {
            accounting: Some(SweepAccounting {
                live_refresh_failures: 2,
                complete: true,
                ..SweepAccounting::default()
            }),
            ..SweepCost::default()
        };
        let segment = SweepCost {
            accounting: Some(SweepAccounting {
                complete: true,
                ..SweepAccounting::default()
            }),
            ..SweepCost::default()
        };
        let snapshots: Vec<_> = [3, 3]
            .into_iter()
            .map(|failures| Workspace::sweep_diagnostics(SweepCost::combine(Some(prior.clone()), Some(segment.clone())), failures).unwrap())
            .collect();
        assert_eq!(snapshots[0].accounting.as_ref().unwrap().live_refresh_failures, 5);
        assert_eq!(snapshots[0].accounting, snapshots[1].accounting);
        let resumed: SweepCost = from_value(to_value(&snapshots[1]).unwrap()).unwrap();
        let completed = Workspace::sweep_diagnostics(SweepCost::combine(Some(resumed), Some(segment)), 1).unwrap();
        assert_eq!(
            completed.accounting.unwrap().live_refresh_failures,
            6,
            "resume adds only its own refresh failures"
        );
    }

    type CaptureScript = Box<dyn FnMut(&Command, &str) -> io::Result<Captured>>;
    type JournalArmHook = Box<dyn FnOnce(&Utf8Path)>;

    std::thread_local! {
        static CAPTURE_SCRIPT: std::cell::RefCell<Option<CaptureScript>> = const { std::cell::RefCell::new(None) };
        static JOURNAL_ARM_HOOK: std::cell::RefCell<Option<JournalArmHook>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn before_journal_arm(base: &Utf8Path) {
        let hook = JOURNAL_ARM_HOOK.with_borrow_mut(Option::take);
        if let Some(hook) = hook {
            hook(base);
        }
    }

    pub(super) fn with_journal_arm_hook<T>(hook: impl FnOnce(&Utf8Path) + 'static, operation: impl FnOnce() -> T) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                JOURNAL_ARM_HOOK.with_borrow_mut(|slot| {
                    let _unused = slot.take();
                });
            }
        }
        JOURNAL_ARM_HOOK.with_borrow_mut(|slot| {
            assert!(slot.is_none(), "journal arming hooks cannot nest on one thread");
            *slot = Some(Box::new(hook));
        });
        let _reset = Reset;
        operation()
    }

    pub(super) fn scripted_capture(command: &Command, what: &str) -> Option<io::Result<Captured>> {
        CAPTURE_SCRIPT.with_borrow_mut(|slot| slot.as_mut().map(|script| script(command, what)))
    }

    fn with_capture_script<T>(script: impl FnMut(&Command, &str) -> io::Result<Captured> + 'static, body: impl FnOnce() -> T) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                CAPTURE_SCRIPT.with_borrow_mut(|slot| {
                    let _ = slot.take();
                });
            }
        }
        CAPTURE_SCRIPT.with_borrow_mut(|slot| {
            assert!(slot.is_none(), "capture scripts cannot nest on one thread");
            *slot = Some(Box::new(script));
        });
        let _reset = Reset;
        body()
    }

    fn capture_success(text: &str) -> Captured {
        Captured {
            succeeded: true,
            stdout: text.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[cfg(windows)]
    fn create_native_junction(link: &Utf8Path, target: &Utf8Path) {
        let output = Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn default_claims_do_not_resolve_paths_for_redirected_cache_comparisons() {
        let directory = private_system_tempdir("workspace-parent-default-claim-short-circuit-");
        let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
        let mut resolutions = 0;
        let locks = claim_cache_with_resolver(&root, &root.join("target"), None, |path| {
            resolutions += 1;
            physical(path)
        })
        .unwrap();
        assert!(locks.redirected.is_none());
        assert_eq!(
            resolutions, 0,
            "an implicit default cache needs no redirected-path canonicalization"
        );
    }

    #[test]
    fn an_unavailable_current_directory_still_removes_leading_current_components_from_relative_paths() {
        let normalized = absolute_with_current_directory(Utf8Path::new("./member"), || Err(io::Error::from(io::ErrorKind::NotFound)));
        assert_eq!(normalized.as_str(), "member");
    }

    #[test]
    fn anchoring_returns_reserved_workspace_runtime_errors_before_saving_the_manifest() {
        let directory = private_system_tempdir("workspace-parent-workspace-runtime-errors-");
        let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
        let source = root.join("source");
        let copy = root.join("copy");
        fs::create_dir(&copy).unwrap();
        let manifest = copy.join("Cargo.toml");
        let text =
            "[package]\nname='subject'\nversion='0.0.0'\n[workspace.dependencies]\ngamma_rt={package='different-package',version='1'}\n";
        fs::write(&manifest, text).unwrap();
        let failure = anchor_manifests(&source, &copy, &copy.join("runtime"))
            .expect_err("an unrelated workspace dependency cannot occupy the reserved runtime name");
        assert!(failure.message().contains("gamma_rt"), "{failure}");
        assert_eq!(fs::read_to_string(manifest).unwrap(), text);
    }

    #[test]
    #[cfg(windows)]
    fn a_linked_default_campaign_is_refused_before_its_target_is_claimed() {
        let directory = private_system_tempdir("workspace-parent-linked-default-");
        let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
        let base = gamma_base(&root, None);
        let real = root.join("real");
        fs::create_dir(&real).unwrap();
        fs::create_dir_all(base.parent().unwrap()).unwrap();
        create_native_junction(&base, &real);
        let failure = claim_default_campaign_cache(&root, &root.join("target"), &base)
            .expect_err("a linked default campaign must return a refusal rather than panic");
        assert!(failure.message().contains("is a link"), "{failure}");
        assert!(!real.join(CACHE_OWNER).exists());
    }

    #[test]
    #[cfg(windows)]
    fn cleaning_rechecks_campaign_identity_after_a_native_workspace_relocation() {
        let directory = private_system_tempdir("workspace-parent-relocated-clean-");
        let outer = absolute(Utf8Path::from_path(directory.path()).unwrap());
        let first = outer.join("first");
        let second = outer.join("second");
        let source = outer.join("source");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        create_native_junction(&source, &first);
        crate::testing::with_cache_home_at(outer.join("private"), || {
            let scratch = gamma_base(&source, None);
            fs::create_dir_all(&scratch).unwrap();
            let retained = scratch.join("retained");
            fs::write(&retained, "keep this cache").unwrap();
            let failure = clean_cache_with_guard(&source, &outer.join("target"), |root, base| {
                validate_default_cache_base(root, base)?;
                fs::remove_dir(&source).unwrap();
                create_native_junction(&source, &second);
                Ok(())
            })
            .expect_err("a root relocated after scratch validation must fail the independent campaign guard");
            assert!(failure.message().contains("escaped its private cache base"), "{failure}");
            assert_eq!(fs::read_to_string(retained).unwrap(), "keep this cache");
        });
    }

    #[test]
    fn relative_owned_campaign_locators_are_not_accepted_as_absolute_campaign_state() {
        let directory = tempfile::Builder::new()
            .prefix("workspace-parent-relative-campaign-")
            .tempdir_in(".")
            .unwrap();
        let root = physical(Utf8Path::from_path(directory.path()).unwrap());
        crate::testing::write_project(
            root.as_std_path(),
            &[
                ("Cargo.toml", "[package]\nname='located'\nversion='0.0.0'\n[workspace]\n"),
                ("src/lib.rs", ""),
            ],
        );
        crate::testing::with_cache_home_at(root.join("private"), || {
            let located = root.join("located");
            fs::create_dir(&located).unwrap();
            mark_cache_owned_for_test(&root, &located);
            let default = gamma_base(&root, None);
            fs::create_dir_all(&default).unwrap();
            mark_cache_owned_for_test(&root, &default);
            let relative = located.strip_prefix(physical(Utf8Path::new("."))).unwrap();
            assert!(!relative.is_absolute());
            fs::write(default.join(CAMPAIGN_LOCATION), relative.as_str()).unwrap();
            let (resolved, base) = campaign_state_from(&root, None).unwrap();
            assert!(same_existing_path(&resolved, &root));
            assert_eq!(base, default);
        });
    }

    #[test]
    #[cfg(windows)]
    fn campaign_and_redirected_inspection_failures_are_not_missing_owners_or_empty_inventories() {
        let directory = private_system_tempdir("workspace-parent-inspection-errors-");
        let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
        crate::testing::write_project(
            root.as_std_path(),
            &[
                ("Cargo.toml", "[package]\nname='inspection'\nversion='0.0.0'\n[workspace]\n"),
                ("src/lib.rs", ""),
            ],
        );
        let denied = root.join("denied");
        fs::write(&denied, "keep").unwrap();
        let held = File::options().read(true).share_mode(0).open(&denied).unwrap();
        let inspect = |_: &Utf8Path| {
            let result = File::open(&denied).and_then(|file| file.metadata());
            assert_ne!(result.as_ref().unwrap_err().kind(), io::ErrorKind::NotFound);
            result
        };
        crate::testing::with_cache_home_at(root.join("private"), || {
            let base = campaign_base(&root, &root.join("target"), None);
            let failure = campaign_state_from_with_metadata(&root, None, inspect)
                .expect_err("a native non-NotFound inspection refusal cannot authorize absent campaign state");
            assert_eq!(
                failure.message(),
                format!("could not inspect the campaign state at `{base}` before validating its owner")
            );
        });
        let base = root.join("redirected");
        fs::create_dir(&base).unwrap();
        fs::write(base.join(CACHE_OWNER), root.as_str()).unwrap();
        let failure = claim_redirected_cache_with(&root, &base, inspect, has_any_entries).unwrap_err();
        assert_eq!(
            failure.message(),
            format!(
                "could not inspect the cargo-gamma cache owner marker at `{}`",
                base.join(CACHE_OWNER)
            )
        );
        drop(held);
        fs::remove_file(base.join(CACHE_OWNER)).unwrap();
        let failure = claim_redirected_cache_with(
            &root,
            &base,
            |owner| fs::symlink_metadata(owner.as_std_path()),
            |_| has_any_entries(&denied),
        )
        .unwrap_err();
        assert_eq!(failure.message(), format!("could not inspect the redirected cache at `{denied}`"));
        assert_eq!(fs::read_to_string(denied).unwrap(), "keep");
    }

    #[test]
    fn campaign_claims_return_postlock_resolution_errors_and_reject_a_changed_workspace() {
        for changed in [false, true] {
            let directory = crate::testing::workdir("workspace-parent-rechecking-");
            let root = Utf8Path::from_path(directory.path()).unwrap();
            let first = root.join("first");
            let second = root.join("second");
            fs::create_dir(&first).unwrap();
            fs::create_dir(&second).unwrap();
            let mut calls = 0;
            let failure = claim_campaign_state_with(&first, None, |_, _| {
                calls += 1;
                if calls == 1 {
                    Ok((first.clone(), first.join("campaign")))
                } else if changed {
                    Ok((second.clone(), second.join("campaign")))
                } else {
                    Err(error!("the campaign disappeared after acquiring its lock"))
                }
            })
            .expect_err("a postlock failure or root change cannot return a successfully claimed campaign");
            assert_eq!(calls, 2);
            if changed {
                assert!(
                    failure.message().contains("while cargo-gamma was acquiring its state lock"),
                    "{failure}"
                );
            } else {
                assert_eq!(failure.message(), "the campaign disappeared after acquiring its lock");
            }
            drop(claim_workspace(&first).expect("an unsuccessful campaign claim releases its workspace lock"));
        }
    }

    #[test]
    fn late_cache_inventory_errors_are_returned_after_a_real_lock_entry() {
        let directory = crate::testing::workdir("workspace-parent-late-inventory-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        fs::write(root.join("lock"), "preserved").unwrap();
        let entry = fs::read_dir(root)
            .unwrap()
            .map(core::result::Result::unwrap)
            .find(|entry| entry.file_name() == "lock")
            .unwrap();
        let native_error = File::open(root.join("absent")).unwrap_err();
        let failure = has_unowned_entries_from(root, [Ok(entry), Err(native_error)])
            .expect_err("a late native entry failure cannot be treated as an empty owned directory");
        assert_eq!(failure.message(), format!("could not inspect an entry in the cache at `{root}`"));
        assert_eq!(fs::read_to_string(root.join("lock")).unwrap(), "preserved");
    }

    #[test]
    #[cfg(windows)]
    fn owner_open_failures_and_redirected_creation_refusals_keep_their_native_boundaries() {
        let directory = crate::testing::workdir("workspace-parent-owner-open-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let cache = root.join("cache");
        fs::create_dir(&cache).unwrap();
        fs::create_dir(cache.join(CACHE_OWNER)).unwrap();
        let failure = validate_cache_owner(root, &cache, CacheKind::Default)
            .expect_err("a directory cannot serve as the regular-file cache owner marker");
        assert_eq!(
            failure.message(),
            format!(
                "could not inspect the cargo-gamma cache owner marker at `{}`",
                cache.join(CACHE_OWNER)
            )
        );
        let file = root.join("file");
        fs::write(&file, "retained").unwrap();
        let redirected = file.join("redirected");
        let failure = claim_redirected_cache(root, &redirected).expect_err("a redirected cache cannot be created beneath a file");
        assert_eq!(
            failure.message(),
            format!("could not create the redirected cargo-gamma cache at `{redirected}`")
        );
        assert_eq!(fs::read_to_string(file).unwrap(), "retained");
    }

    #[test]
    fn vcs_exposure_preserves_git_pointers_and_returns_each_native_refusal_stage() {
        for boundary in ["git", "malformed", "write", "hg"] {
            let directory = private_system_tempdir("workspace-parent-vcs-exposure-");
            let root = Utf8Path::from_path(directory.path()).unwrap();
            let source = root.join("source");
            let scratch = root.join("scratch");
            fs::create_dir(&source).unwrap();
            fs::create_dir(&scratch).unwrap();
            if boundary == "hg" {
                fs::create_dir(source.join(".hg")).unwrap();
                fs::create_dir(scratch.join(".hg")).unwrap();
                let failure = expose_vcs_metadata(&source, &scratch).unwrap_err();
                assert!(failure.message().starts_with("could not expose VCS metadata at"), "{failure}");
                continue;
            }
            let git = source.join(".git");
            if boundary == "malformed" {
                fs::write(&git, "a pointer without its required prefix").unwrap();
            } else {
                fs::create_dir(&git).unwrap();
            }
            if boundary == "write" {
                fs::create_dir(scratch.join(".git")).unwrap();
            }
            let result = expose_vcs_metadata(&source, &scratch);
            match boundary {
                "git" => {
                    result.unwrap();
                    assert_eq!(fs::read_to_string(scratch.join(".git")).unwrap(), format!("gitdir: {git}\n"));
                }
                "malformed" => assert_eq!(
                    result.unwrap_err().message(),
                    format!("Git metadata pointer `{git}` has no `gitdir:` target")
                ),
                "write" => assert_eq!(
                    result.unwrap_err().message(),
                    format!("could not expose Git metadata at `{}`", scratch.join(".git"))
                ),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn vcs_exposure_visits_later_kinds_and_selects_native_link_shapes_before_reporting_link_errors() {
        for fail in [false, true] {
            let directory = private_system_tempdir("workspace-parent-vcs-link-kinds-");
            let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
            let source = root.join("source");
            let scratch = root.join("scratch");
            fs::create_dir(&source).unwrap();
            fs::create_dir(&scratch).unwrap();
            fs::create_dir(source.join(".git")).unwrap();
            fs::create_dir(source.join(".hg")).unwrap();
            fs::write(source.join(".bzr"), "file-shaped metadata").unwrap();
            let mut observations = Vec::new();
            let result = expose_vcs_metadata_with(&source, &scratch, |marker, destination, directory| {
                observations.push((marker.file_name().unwrap().to_owned(), directory));
                assert_eq!(destination.parent(), Some(scratch.as_path()));
                if fail {
                    Err(File::open(root.join("absent")).unwrap_err())
                } else {
                    fs::write(destination, format!("directory={directory}"))
                }
            });
            assert_eq!(
                fs::read_to_string(scratch.join(".git")).unwrap(),
                format!("gitdir: {}\n", source.join(".git"))
            );
            if fail {
                assert_eq!(observations, [(".hg".to_owned(), true)]);
                assert_eq!(
                    result.unwrap_err().message(),
                    format!("could not expose VCS metadata at `{}`", scratch.join(".hg"))
                );
            } else {
                result.unwrap();
                assert_eq!(observations, [(".hg".to_owned(), true), (".bzr".to_owned(), false)]);
                assert_eq!(fs::read_to_string(scratch.join(".hg")).unwrap(), "directory=true");
                assert_eq!(fs::read_to_string(scratch.join(".bzr")).unwrap(), "directory=false");
            }
        }
    }

    #[test]
    fn preparation_exposes_metadata_only_for_the_default_cache_and_returns_invalid_git_pointers() {
        for boundary in ["default", "explicit", "malformed"] {
            let directory = private_system_tempdir("workspace-parent-vcs-preparation-");
            let source = absolute(Utf8Path::from_path(directory.path()).unwrap());
            crate::testing::write_project(
                source.as_std_path(),
                &[
                    ("Cargo.toml", "[package]\nname='vcs'\nversion='0.0.0'\n[workspace]\n"),
                    ("src/lib.rs", ""),
                ],
            );
            let git = source.join(".git");
            if boundary == "malformed" {
                fs::write(&git, "invalid git pointer").unwrap();
            } else {
                fs::create_dir(&git).unwrap();
            }
            let config = Config {
                cache_dir: (boundary == "explicit").then(|| source.join("explicit-cache")),
                ..Config::default()
            };
            let result = Workspace::prepare(&source, &source.join("target"), &config, &mut crate::testing::Recorder::default());
            if boundary == "malformed" {
                assert_eq!(
                    result.unwrap_err().message(),
                    format!("Git metadata pointer `{git}` has no `gitdir:` target")
                );
            } else {
                let mut work = result.unwrap();
                assert_eq!(work.root.join(".git").exists(), boundary == "default");
                work.teardown().unwrap();
            }
        }
    }

    #[test]
    fn a_competing_owner_marker_is_never_overwritten_after_the_empty_inventory_check() {
        let directory = private_system_tempdir("workspace-parent-owner-race-");
        let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
        let base = root.join("cache");
        fs::create_dir(&base).unwrap();
        let owner = base.join(CACHE_OWNER);
        let failure = validate_cache_owner_with_inventory(&root, &base, CacheKind::Default, File::metadata, |base| {
            assert!(!has_unowned_entries(base).unwrap());
            fs::write(&owner, "competing owner").unwrap();
            Ok(false)
        })
        .expect_err("create_new must return a concurrent ownership refusal rather than truncate or panic");
        assert_eq!(
            failure.message(),
            format!("could not write the cargo-gamma cache owner marker at `{owner}`")
        );
        assert_eq!(fs::read_to_string(owner).unwrap(), "competing owner");
    }

    #[test]
    fn claiming_an_owner_in_a_missing_cache_returns_the_native_inventory_error() {
        let directory = private_system_tempdir("workspace-parent-missing-inventory-");
        let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
        let base = root.join("absent");
        let failure = validate_cache_owner(&root, &base, CacheKind::Default)
            .expect_err("a missing cache cannot be enumerated while establishing ownership");
        assert_eq!(failure.message(), format!("could not inspect the cache at `{base}`"));
        assert!(!base.exists());
    }

    #[test]
    #[cfg(windows)]
    fn native_read_denials_keep_git_pointer_and_cache_owner_failure_stages_distinct() {
        let directory = private_system_tempdir("workspace-parent-native-read-denial-");
        let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
        let source = root.join("source");
        let scratch = root.join("scratch");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&scratch).unwrap();
        let git = source.join(".git");
        fs::write(&git, "gitdir: elsewhere").unwrap();
        let held = File::options().read(true).share_mode(0).open(&git).unwrap();
        let failure = expose_vcs_metadata(&source, &scratch).unwrap_err();
        assert_eq!(failure.message(), format!("could not read Git metadata pointer `{git}`"));
        drop(held);
        let owner = scratch.join(CACHE_OWNER);
        fs::write(&owner, source.as_str()).unwrap();
        let held = File::options().read(true).share_mode(0).open(&owner).unwrap();
        let failure = validate_cache_owner(&source, &scratch, CacheKind::Default).unwrap_err();
        assert_eq!(
            failure.message(),
            format!("could not inspect the cargo-gamma cache owner marker at `{owner}`")
        );
        drop(held);
    }

    #[test]
    #[cfg(windows)]
    fn native_junction_aliases_cannot_turn_copy_refusals_into_prunable_layouts() {
        let directory = private_system_tempdir("workspace-parent-junction-pruning-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let source = root.join("source");
        fs::create_dir(&source).unwrap();
        let inside = source.join("inside");
        fs::create_dir(&inside).unwrap();
        for (index, (link, target)) in [
            (root.join("outside-alias"), inside.clone()),
            (source.join("inside-alias"), inside),
            (root.join("root-alias"), source.clone()),
        ]
        .into_iter()
        .enumerate()
        {
            create_native_junction(&link, &target);
            let failure = ensure_copy_terminates(&source, &link)
                .expect_err("a junction that aliases the source cannot be pruned by its lexical spelling");
            assert!(
                failure.message().contains(if index == 2 {
                    "workspace at"
                } else {
                    "not named as a path inside it"
                }),
                "{failure}"
            );
            if index == 2 {
                assert!(failure.message().contains("itself, so copying"), "{failure}");
            }
        }
    }

    #[test]
    #[cfg(windows)]
    fn manifest_lookup_rejects_external_junctions_and_stops_before_an_in_root_physical_ancestor() {
        let directory = private_system_tempdir("workspace-parent-manifest-junctions-");
        let outer = absolute(Utf8Path::from_path(directory.path()).unwrap());
        let source = outer.join("source");
        let external = outer.join("external");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::create_dir_all(external.join("src")).unwrap();
        fs::write(external.join("Cargo.toml"), "[package]\nname='external'\nversion='0.0.0'\n").unwrap();
        let nested = outer.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("Cargo.toml"), "[workspace]\n").unwrap();
        let alias = source.join("alias");
        let scratch = nested.join("scratch");
        for (link, target) in [(&alias, &external), (&scratch, &outer)] {
            create_native_junction(link, target);
        }
        for (root, path) in [(&source, "alias/src/lib.rs"), (&scratch, "source/src/lib.rs")] {
            let work = Workspace::adopt(root.clone(), outer.join("artifacts"));
            let files = [TargetFile {
                path: path.into(),
                absolute: root.join(path),
                package: "subject".into(),
                source: None,
            }];
            assert_eq!(work.manifest_of("subject", &files), None);
        }
    }

    #[test]
    fn nextest_packages_are_sorted_before_nonadjacent_duplicates_are_removed() {
        let work = unsettled_default();
        let binaries = ["z-last", "a-first", "z-last"].map(|package| TestBinary {
            package_id: package.to_owned(),
            ..crate::testing::test_binary("unused")
        });
        let command = work.nextest_list_command(&binaries);
        let arguments = command.get_args().map(|argument| argument.to_str().unwrap()).collect::<Vec<_>>();
        let packages = arguments
            .windows(2)
            .filter(|pair| pair[0] == "--package")
            .map(|pair| pair[1])
            .collect::<Vec<_>>();
        assert_eq!(packages, ["a-first", "z-last"]);
    }

    #[test]
    fn native_cache_readers_and_workspace_claims_return_nondirectory_errors() {
        let directory = crate::testing::workdir("workspace-parent-native-errors-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let file = root.join("plain-file");
        fs::write(&file, "retained").unwrap();
        for unowned in [false, true] {
            let result = if unowned {
                has_unowned_entries(&file)
            } else {
                has_any_entries(&file)
            };
            let failure = result.expect_err("a file cannot be enumerated as a cache directory");
            assert!(failure.message().contains(file.as_str()), "{failure}");
            assert!(failure.message().starts_with("could not inspect"), "{failure}");
        }
        let base = gamma_base(root, None);
        let namespace = base.parent().unwrap();
        fs::create_dir_all(namespace.parent().unwrap()).unwrap();
        fs::write(namespace, "not a namespace directory").unwrap();
        let failure = claim_workspace_at(root, &base).expect_err("a structurally valid cache cannot be created beneath a file");
        assert_eq!(
            failure.message(),
            format!("could not create cargo-gamma's workspace cache at `{base}`")
        );
        assert_eq!(fs::read_to_string(file).unwrap(), "retained");
    }

    #[test]
    fn default_cache_guards_check_the_namespace_private_home_and_campaign_identity_independently() {
        let directory = crate::testing::workdir("workspace-parent-shape-guards-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let correct = gamma_base(root, None);
        validate_default_cache_base(root, &correct).unwrap();
        let misplaced = root.join("wrong-namespace").join(workspace_identity(root));
        let failure = validate_default_cache_base(root, &misplaced).unwrap_err();
        assert!(failure.message().contains("malformed default cache path"), "{failure}");
        let outside = root.join("outside-home").join("cargo-gamma").join(workspace_identity(root));
        let failure = validate_default_cache_base(root, &outside).unwrap_err();
        assert!(failure.message().contains("escaped its private cache home"), "{failure}");
        validate_default_campaign_base(root, &root.join("target"), &correct).unwrap();
        let failure = validate_default_campaign_base(root, &root.join("target"), &outside).unwrap_err();
        assert!(failure.message().contains("escaped its private cache base"), "{failure}");
        let failure = claim_default_campaign_cache(root, &root.join("target"), &outside).unwrap_err();
        assert!(failure.message().contains("escaped its private cache base"), "{failure}");
        assert!(!outside.exists());
    }

    #[test]
    fn preparing_a_workspace_propagates_an_existing_claim_instead_of_panicking() {
        let directory = crate::testing::workdir("workspace-parent-held-claim-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        crate::testing::write_project(
            root.as_std_path(),
            &[
                ("Cargo.toml", "[package]\nname='claimed'\nversion='0.0.0'\n[workspace]\n"),
                ("src/lib.rs", ""),
            ],
        );
        let _held = claim_workspace(root).unwrap();
        let failure = Workspace::prepare(
            root,
            &root.join("target"),
            &Config::default(),
            &mut crate::testing::Recorder::default(),
        )
        .expect_err("an active workspace lock is a returned usage error, not a panic");
        assert!(failure.is_usage());
        assert!(failure.message().contains("another `cargo gamma` run"), "{failure}");
    }

    #[test]
    fn preparation_observes_the_exact_adopted_lock_identity_before_copying() {
        let directory = crate::testing::workdir("workspace-parent-lock-handoff-");
        let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
        crate::testing::write_project(
            root.as_std_path(),
            &[
                ("Cargo.toml", "[package]\nname='handoff'\nversion='0.0.0'\n[workspace]\n"),
                ("src/lib.rs", ""),
            ],
        );
        let target = root.join("target");
        let locks = claim_cache(&root, &target, None).unwrap();
        let expected = cache_lock_identity(&locks);
        let observed = std::cell::Cell::new(None);
        let mut work = Workspace::prepare_with_locks_observed(
            &root,
            &target,
            &Config::default(),
            &mut crate::testing::Recorder::default(),
            Some(locks),
            |locks| observed.set(Some(cache_lock_identity(locks))),
        )
        .unwrap();
        assert_eq!(observed.get(), Some(expected));
        work.teardown().unwrap();
    }

    #[test]
    fn a_wrapping_lock_identity_sequence_does_not_skip_into_the_next_retained_identity() {
        let retained = next_cache_lock_identity(&AtomicUsize::new(1));
        let next = AtomicUsize::new(usize::MAX);
        let last = next_cache_lock_identity(&next);
        let wrapped = next_cache_lock_identity(&next);
        assert_ne!(last, retained);
        assert_ne!(wrapped, last);
        assert_ne!(wrapped, retained);
    }

    #[test]
    fn preparation_and_cleaning_return_private_home_guard_refusals_without_writing() {
        let directory = crate::testing::workdir("workspace-parent-home-refusal-");
        let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
        let home = root.join("missing").join("..").join("private");
        crate::testing::with_cache_home_at(home, || {
            let failure = Workspace::prepare(
                &root,
                &root.join("target"),
                &Config::default(),
                &mut crate::testing::Recorder::default(),
            )
            .expect_err("a path outside the exact private-home spelling must be rejected before copying");
            assert!(failure.message().contains("escaped its private cache home"), "{failure}");
            let failure = clean_cache(&root, &root.join("target"))
                .expect_err("cleaning must return the same private-home guard refusal rather than panic");
            assert!(failure.message().contains("escaped its private cache home"), "{failure}");
        });
        assert!(!root.join("private").exists());
        assert!(!root.join("target").exists());
    }

    #[test]
    fn runtime_linking_returns_malformed_manifest_and_dependency_shape_errors() {
        for malformed in [false, true] {
            let directory = crate::testing::workdir("workspace-parent-runtime-errors-");
            let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
            fs::create_dir(root.join("src")).unwrap();
            fs::write(root.join("src/lib.rs"), "pub fn subject() {}\n").unwrap();
            let manifest = root.join("Cargo.toml");
            let text = if malformed {
                "[package\n"
            } else {
                "[package]\nname='subject'\nversion='0.0.0'\n[target.'cfg(windows)'.dependencies]\ngamma_rt={package='different-package',version='1'}\n"
            };
            fs::write(&manifest, text).unwrap();
            let work = Workspace::adopt(root.clone(), root.join("target"));
            let files = [TargetFile {
                path: "src/lib.rs".into(),
                absolute: root.join("src/lib.rs"),
                package: "subject".into(),
                source: None,
            }];
            let failure = work
                .link_runtime("subject", &files)
                .expect_err("invalid manifests cannot be linked to a runtime");
            assert!(
                failure.message().contains(if malformed { "could not parse" } else { "gamma_rt" }),
                "{failure}"
            );
            assert_eq!(fs::read_to_string(manifest).unwrap(), text);
        }
    }

    #[test]
    fn checkpoint_publication_refusal_is_returned_after_the_journal_was_armed() {
        let directory = crate::testing::workdir("workspace-parent-journal-refusal-");
        let base = Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let mut work = Workspace::adopt(base.join("workspace"), base.join("target"));
        fs::write(base.join("testing-session.json"), "{}").unwrap();
        fs::write(base.join("testing-verdicts.jsonl"), "").unwrap();
        work.arm_testing_journal().unwrap();
        let mut mutant = crate::fixtures::mutant();
        mutant.outcome = crate::model::Outcome::Survived;
        work.testing_journal
            .as_mut()
            .unwrap()
            .replace_writer_for_test(File::open(base.join("testing-verdicts.jsonl")).unwrap());
        let failure = work
            .checkpoint_mutant(
                &mutant,
                &super::super::killers::Killers::default(),
                &super::super::session::SweepCost::default(),
            )
            .expect_err("the native read-only journal handle must refuse appending a checkpoint");
        assert!(failure.message().starts_with("could not persist testing progress"), "{failure}");
    }

    #[test]
    fn metadata_owner_diagnostics_and_explicit_base_resolution_keep_the_requested_cache() {
        let directory = crate::testing::workdir("workspace-parent-state-owner-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        crate::testing::write_project(
            root.as_std_path(),
            &[
                ("Cargo.toml", "[package]\nname='subject'\nversion='0.0.0'\n[workspace]\n"),
                ("src/lib.rs", ""),
            ],
        );
        let base = root.join("explicit-cache");
        fs::create_dir(&base).unwrap();
        let failure = validate_campaign_state_owner(root, &base).unwrap_err();
        assert!(failure.message().ends_with("(owner marker resolves to `<unowned>`)"), "{failure}");
        mark_cache_owned_for_test(root, &base);
        assert!(same_existing_path(
            &campaign_base_from_state(root, Some(&base)).unwrap(),
            &physical(&base)
        ));
        let held = claim_workspace(root).unwrap();
        let failure = claim_campaign_state(root, Some(&base)).expect_err("a campaign claim must respect the active workspace lock");
        assert!(failure.is_usage());
        drop(held);
        let missing = root.join("no-workspace");
        fs::create_dir(&missing).unwrap();
        fs::write(missing.join("Cargo.toml"), "[broken").unwrap();
        claim_campaign_state(&missing, None).expect_err("a malformed workspace cannot resolve campaign state");
        claim_testing_state(&missing, Some(&root.join("unowned"))).expect_err("an ownerless cache cannot resolve testing state");
    }

    #[test]
    fn refused_campaign_locators_report_the_failure_without_claiming_publication() {
        let directory = crate::testing::workdir("workspace-parent-locator-refusal-");
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let base = root.join("campaign");
        fs::create_dir(&base).unwrap();
        assert!(!remember_campaign_base(root, &base));
        mark_cache_owned_for_test(root, &base);
        let locator = gamma_base(root, None).join(CAMPAIGN_LOCATION);
        fs::create_dir_all(&locator).unwrap();
        let run = crate::notes::Run::new();
        let _scope = crate::notes::enter(Some(&run));
        assert!(!remember_campaign_base(root, &base));
        let notes = crate::notes::drain();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("could not save campaign-state location:"), "{notes:?}");
    }

    #[test]
    fn absent_binary_parents_do_not_hide_later_test_search_paths() {
        let mut work = unsettled_default();
        work.libraries.clear();
        let binaries = [crate::testing::test_binary(""), crate::testing::test_binary("profile/deps/actual")];
        work.prepend_test_search_paths(&binaries);
        assert_eq!(work.libraries, ["profile", "profile/deps"].map(Utf8PathBuf::from));
    }

    #[test]
    #[cfg(windows)]
    fn anchored_manifests_keep_external_paths_and_return_parse_and_write_refusals() {
        for boundary in ["paths", "parse", "write"] {
            let directory = crate::testing::workdir("workspace-parent-anchor-contract-");
            let source = Utf8Path::from_path(directory.path()).unwrap().join("source");
            let copy = source.parent().unwrap().join("copy");
            fs::create_dir_all(&source).unwrap();
            fs::create_dir(&copy).unwrap();
            let external = source.parent().unwrap().join("external");
            fs::create_dir(&external).unwrap();
            fs::write(external.join("Cargo.toml"), "[package]\nname='external'\nversion='0.0.0'\n").unwrap();
            let manifest = copy.join("Cargo.toml");
            let text = if boundary == "parse" {
                "[package\n"
            } else {
                "[package]\nname='subject'\nversion='0.0.0'\n[dependencies]\nexternal={path='../external'}\n[workspace]\n"
            };
            fs::write(&manifest, text).unwrap();
            let _denied = (boundary == "write").then(|| fs::OpenOptions::new().read(true).share_mode(1).open(&manifest).unwrap());
            let result = anchor_manifests(&source, &copy, &copy.join("runtime"));
            match boundary {
                "paths" => {
                    result.unwrap();
                    let saved = fs::read_to_string(&manifest).unwrap();
                    let saved = saved.parse::<toml_edit::DocumentMut>().unwrap();
                    let dependency = Utf8Path::new(saved["dependencies"]["external"]["path"].as_str().unwrap());
                    assert!(dependency.is_absolute(), "{dependency}");
                    assert_eq!(absolute(dependency), absolute(&external));
                }
                "parse" => assert!(result.unwrap_err().message().contains("could not parse")),
                "write" => {
                    let failure = result.unwrap_err();
                    assert_eq!(failure.message(), format!("could not update `{manifest}`"));
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn cargo_capture_callers_preserve_command_identity_labels_output_and_each_independent_failure() {
        let work = unsettled_default();
        let root = work.root.clone();
        for metadata in [true, false] {
            for mode in 0..4 {
                let expected_root = root.clone();
                let observed = std::rc::Rc::new(std::cell::Cell::new(0));
                let calls = std::rc::Rc::clone(&observed);
                let label = if metadata { "cargo metadata" } else { "cargo nextest list" };
                let result = with_capture_script(
                    move |command, what| {
                        calls.set(calls.get() + 1);
                        assert_eq!(command.get_current_dir(), Some(expected_root.as_std_path()));
                        assert_eq!(what, label);
                        let args = command.get_args().map(OsStr::to_string_lossy).collect::<Vec<_>>();
                        if metadata {
                            assert_eq!(args, ["metadata", "--format-version", "1"]);
                        } else {
                            assert_eq!(
                                args,
                                ["nextest", "list", "--list-type", "binaries-only", "--message-format", "json"]
                            );
                        }
                        match mode {
                            0 => Ok(capture_success("distinctive payload\n")),
                            1 => Err(io::Error::other("scripted transport refusal")),
                            2 => Ok(Captured {
                                succeeded: false,
                                stdout: b"not successful output".to_vec(),
                                stderr: b"  scripted status refusal \n".to_vec(),
                            }),
                            _ => Ok(Captured {
                                succeeded: true,
                                stdout: vec![0xff],
                                stderr: Vec::new(),
                            }),
                        }
                    },
                    || {
                        if metadata {
                            work.capture_cargo_metadata()
                        } else {
                            work.capture_nextest_list(&[])
                        }
                    },
                );
                assert_eq!(observed.get(), 1);
                match mode {
                    0 => assert_eq!(result.unwrap(), "distinctive payload\n"),
                    1 => assert_eq!(result.unwrap_err().message(), format!("could not run `{label}` in {root}")),
                    2 => assert_eq!(
                        result.unwrap_err().message(),
                        format!("`{label}` failed in {root}:\nscripted status refusal")
                    ),
                    _ => assert_eq!(result.unwrap_err().message(), format!("`{label}` did not print valid UTF-8")),
                }
            }
        }
    }

    #[test]
    fn arming_the_real_cargo_environment_publishes_metadata_paths_and_package_variables_together() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let mut work = unsettled_at(root.clone(), root.join("runtime"));
        let mut binary = crate::testing::test_binary("release/deps/prepared");
        binary.package_id = "proof-package".into();
        let metadata = json!({
            "packages": [{
                "id": "proof-package", "name": "prepared", "version": "2.3.4",
                "manifest_path": root.join("Cargo.toml"),
            }],
        })
        .to_string();
        let artifacts = json!({
            "reason": "build-script-executed", "package_id": "proof-package", "out_dir": "generated-output",
        })
        .to_string();
        let output = metadata.clone();
        with_capture_script(
            move |_, what| {
                assert_eq!(what, "cargo metadata");
                Ok(capture_success(&output))
            },
            || work.arm_test_environment(std::slice::from_ref(&binary), &artifacts),
        )
        .unwrap();
        assert_eq!(work.cargo_metadata.as_deref(), Some(metadata.as_str()));
        assert_eq!(work.libraries, ["release", "release/deps"].map(Utf8PathBuf::from));
        assert_eq!(
            work.testing_roots().unwrap(),
            [root.clone(), root.join("runtime"), "generated-output".into()]
        );
        let mut command = Command::new("not-executed");
        work.configure_test_environment(&mut command, &binary);
        assert_eq!(command_env(&command, "CARGO_PKG_NAME"), "prepared");
        assert_eq!(command_env(&command, "CARGO_PKG_VERSION"), "2.3.4");
        assert_eq!(command_env(&command, "OUT_DIR"), "generated-output");
    }

    #[test]
    fn arming_failures_retain_the_previous_environment_and_metadata_after_settling_search_paths() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
        for transport in [false, true] {
            let mut work = unsettled_at(root.clone(), root.join("runtime"));
            let binary = crate::testing::test_binary("release/deps/prepared");
            work.cargo_metadata = Some("previous metadata".into());
            work.set_test_environment(&binary.package_id, "PREVIOUS", "retained");
            let result = with_capture_script(
                move |_, _| {
                    if transport {
                        Err(io::Error::other("metadata transport refused"))
                    } else {
                        Ok(capture_success("malformed metadata"))
                    }
                },
                || work.arm_test_environment(std::slice::from_ref(&binary), ""),
            );
            let failure = result.expect_err("both transport refusal and malformed metadata must abort environment preparation");
            if transport {
                assert_eq!(failure.message(), format!("could not run `cargo metadata` in {root}"));
            } else {
                assert_eq!(failure.message(), "could not decode Cargo metadata for the test environment");
            }
            assert_eq!(work.cargo_metadata.as_deref(), Some("previous metadata"));
            assert_eq!(work.libraries, ["release", "release/deps"].map(Utf8PathBuf::from));
            let mut command = Command::new("not-executed");
            work.configure_test_environment(&mut command, &binary);
            assert_eq!(command_env(&command, "PREVIOUS"), "retained");
        }
    }

    #[test]
    fn arming_nextest_retains_both_real_scratch_documents_and_propagates_late_listing_failure() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let mut work = unsettled_at(root.clone(), root.join("runtime"));
        work.target = root.join("target");
        work.cargo_metadata = Some("chosen metadata".into());
        with_capture_script(
            |_, label| {
                assert_eq!(label, "cargo nextest list");
                Ok(capture_success("{\"rust-suites\":{}}"))
            },
            || work.arm_nextest(&[]),
        )
        .unwrap();
        assert!(work.runner().is_some());
        let paths = [work.target.join("nextest-binaries.json"), work.target.join("nextest-metadata.json")];
        assert_eq!(fs::read_to_string(&paths[0]).unwrap(), "{\"rust-suites\":{}}");
        assert_eq!(fs::read_to_string(&paths[1]).unwrap(), "chosen metadata");
        assert_eq!(
            work.testing_roots().unwrap(),
            [root.clone(), root.join("runtime"), paths[0].clone(), paths[1].clone()]
        );
        let failure = with_capture_script(
            |_, _| Err(io::Error::other("late nextest listing refusal")),
            || work.arm_nextest(&[]),
        )
        .expect_err("an armed runner does not authorize suppressing a later failed listing");
        assert_eq!(failure.message(), format!("could not run `cargo nextest list` in {root}"));
        assert!(work.runner().is_some(), "the previous runner survives failed replacement");
    }

    #[test]
    fn nextest_preparation_propagates_each_scratch_write_failure_and_rejects_a_missing_binary_after_both_writes() {
        for failure_at in 0..3 {
            let directory = tempfile::tempdir_in(".").unwrap();
            let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
            let mut work = Workspace::adopt(root.clone(), root.join("target"));
            work.cargo_metadata = Some("selected metadata".into());
            fs::create_dir_all(&work.target).unwrap();
            let binaries_path = work.target.join("nextest-binaries.json");
            let metadata_path = work.target.join("nextest-metadata.json");
            if failure_at < 2 {
                fs::create_dir(if failure_at == 0 { &binaries_path } else { &metadata_path }).unwrap();
            }
            let binaries = if failure_at == 2 {
                vec![crate::testing::test_binary("missing-binary")]
            } else {
                vec![]
            };
            let failure = with_capture_script(
                |_, label| {
                    assert_eq!(label, "cargo nextest list");
                    Ok(capture_success("{\"rust-binaries\":{}}"))
                },
                || work.arm_nextest(&binaries),
            )
            .expect_err("the exact scratch/identity failure must escape nextest preparation");
            if failure_at < 2 {
                let blocked = if failure_at == 0 { &binaries_path } else { &metadata_path };
                assert_eq!(failure.message(), format!("could not write `{blocked}`"));
                assert!(blocked.is_dir());
            } else {
                assert_eq!(
                    failure.message(),
                    "`cargo nextest` does not know the test binary `missing-binary` that this run built.\nRun without `--nextest` to run the binaries directly instead."
                );
                assert_eq!(fs::read_to_string(&metadata_path).unwrap(), "selected metadata");
            }
            if failure_at > 0 {
                assert_eq!(fs::read_to_string(&binaries_path).unwrap(), "{\"rust-binaries\":{}}");
            }
            assert!(work.runner().is_none());
        }
    }

    #[test]
    fn an_adopted_workspace_has_no_cargo_metadata_and_refuses_nextest_before_any_capture() {
        let mut work = unsettled_default();
        let failure = with_capture_script(
            |_, _| panic!("an adopted but unarmed workspace must not ask Cargo or nextest anything"),
            || work.arm_nextest(&[]),
        )
        .expect_err("adoption does not imply a prepared Cargo test environment");
        assert_eq!(failure.message(), "the Cargo test environment was not prepared before nextest");
        assert!(work.runner().is_none());
    }

    #[test]
    fn capture_generation_switches_from_ready_to_unavailable_and_back_without_reusing_stale_records() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let mut work = Workspace::adopt(root.clone(), root.join("target"));
        let captures = root.join("captures");
        fs::create_dir(&captures).unwrap();
        fs::write(captures.join("previous.json"), "old generation").unwrap();
        work.set_rustc_captures(Some(captures.clone()));
        assert_eq!(work.rustc_captures(), Some(captures.clone()));
        work.begin_build_sequence();
        assert_eq!(work.rustc_captures(), Some(captures.clone()));
        assert!(fs::read_dir(&captures).unwrap().next().is_none());
        fs::remove_dir(&captures).unwrap();
        fs::write(&captures, "not a directory").unwrap();
        work.begin_build_sequence();
        assert_eq!(work.rustc_captures(), None);
        assert_eq!(fs::read_to_string(&captures).unwrap(), "not a directory");
        fs::remove_file(&captures).unwrap();
        work.begin_build_sequence();
        assert_eq!(work.rustc_captures(), Some(captures));
    }

    #[test]
    fn workspace_resource_capacity_snapshot_exposes_the_actual_named_admission_limit() {
        let mut work = unsettled_default();
        let binary = crate::testing::test_binary("resource-subject");
        work.resources = Resources::fake_tests(&binary, &[("one", "database"), ("two", "database")]);
        assert_eq!(
            work.resource_capacities(),
            std::iter::once((Arc::<str>::from("database"), 1)).collect()
        );
    }

    #[test]
    fn postopen_owner_validation_rejects_a_native_directory_metadata_snapshot_before_reading_marker_bytes() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let base = root.join("cache");
        fs::create_dir(&base).unwrap();
        let owner = base.join(CACHE_OWNER);
        fs::write(&owner, b"\xff").unwrap();
        let inspected = std::cell::Cell::new(false);
        let failure = validate_cache_owner_with_metadata(&root, &base, CacheKind::Default, |file| {
            assert!(file.metadata().unwrap().is_file(), "the real marker must already have been opened");
            inspected.set(true);
            fs::metadata(&base)
        })
        .expect_err("the metadata adapter's nonregular snapshot is refused before marker parsing");
        assert!(inspected.get());
        assert_eq!(
            failure.message(),
            format!(
                "the cargo-gamma cache owner marker at `{owner}` is not a regular file.\n{}",
                CacheKind::Default.collision_advice()
            )
        );
        assert!(failure.is_usage());
        assert_eq!(fs::read(&owner).unwrap(), b"\xff");
    }

    #[test]
    fn owned_cache_cleaning_propagates_independent_directory_open_and_late_entry_failures_after_validating_the_owner() {
        for entry_failure in [false, true] {
            let directory = tempfile::tempdir_in(".").unwrap();
            let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
            let base = root.join("cache");
            fs::create_dir(&base).unwrap();
            let owner = base.join(CACHE_OWNER);
            fs::write(&owner, physical(&root).as_str()).unwrap();
            let inspected = std::cell::Cell::new(false);
            let failure = clean_owned_cache_with_entries(&root, &base, true, |path| {
                assert_eq!(path, base);
                let entries = fs::read_dir(path)?.collect::<Vec<_>>();
                assert_eq!(entries.len(), 1, "only the validated owner marker is present");
                inspected.set(true);
                if entry_failure {
                    Ok(entries
                        .into_iter()
                        .chain(std::iter::once(Err(io::Error::other("late cache entry refusal"))))
                        .collect::<Vec<_>>())
                } else {
                    Err(io::Error::other("cache directory refused after native open"))
                }
            })
            .expect_err("neither failed directory enumeration nor a failed later entry may report a clean cache");
            assert!(inspected.get());
            assert_eq!(
                failure.message(),
                if entry_failure {
                    format!("could not read an entry in cargo-gamma's cache at `{base}`")
                } else {
                    format!("could not read cargo-gamma's cache at `{base}`")
                }
            );
            assert_eq!(fs::read_to_string(&owner).unwrap(), physical(&root).as_str());
        }
    }

    #[test]
    fn cleaning_refuses_native_nondirectory_and_foreign_owner_shapes_before_removing_any_bytes() {
        for foreign_owner in [false, true] {
            let directory = tempfile::tempdir_in(".").unwrap();
            let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
            let base = root.join("cache");
            if foreign_owner {
                fs::create_dir(&base).unwrap();
                fs::write(base.join(CACHE_OWNER), physical(&root.join("foreign")).as_str()).unwrap();
                fs::write(base.join("payload"), "foreign bytes").unwrap();
            } else {
                fs::write(&base, "not a cache directory").unwrap();
            }
            let failure = clean_owned_cache(&root, &base, true).expect_err("neither native shape authorizes destructive cleaning");
            assert!(
                failure.message().contains(if foreign_owner {
                    "belongs to the workspace"
                } else {
                    "is not a directory"
                }),
                "{failure}"
            );
            assert_eq!(
                if foreign_owner {
                    fs::read_to_string(base.join("payload"))
                } else {
                    fs::read_to_string(&base)
                }
                .unwrap(),
                if foreign_owner { "foreign bytes" } else { "not a cache directory" }
            );
        }
    }

    #[test]
    fn cached_entry_type_inspection_failure_keeps_the_real_entry_and_reports_its_exact_path() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let path = root.join("preserved");
        fs::write(&path, "owned bytes").unwrap();
        let entry = fs::read_dir(root).unwrap().next().unwrap().unwrap();
        let failure = remove_cached_with_type(&entry.path(), || {
            assert!(entry.file_type().unwrap().is_file());
            Err(io::Error::other("late cached entry type refusal"))
        })
        .expect_err("a failed late type inspection must not silently unlink or unwrap");
        assert_eq!(failure.message(), format!("could not inspect cached data at `{path}`"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "owned bytes");
    }

    #[test]
    #[cfg(windows)]
    fn owned_cache_cleaning_propagates_native_entry_removal_denial_and_keeps_the_owner_marker() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap().to_owned();
        let base = root.join("cache");
        fs::create_dir(&base).unwrap();
        let owner = base.join(CACHE_OWNER);
        fs::write(&owner, physical(&root).as_str()).unwrap();
        let path = base.join("locked-payload");
        fs::write(&path, "owned locked bytes").unwrap();
        let locked = fs::File::options().read(true).share_mode(0).open(&path).unwrap();
        let failure = clean_owned_cache(&root, &base, true).expect_err("a locked real entry cannot count as removed");
        assert_eq!(failure.message(), format!("could not remove cached data at `{path}`"));
        drop(locked);
        assert_eq!(fs::read_to_string(&path).unwrap(), "owned locked bytes");
        assert_eq!(fs::read_to_string(&owner).unwrap(), physical(&root).as_str());
    }

    /// A capture that succeeded hands back exactly what the command printed.
    #[test]
    fn a_successful_capture_returns_what_the_command_printed() {
        let captured: io::Result<Captured> = Ok(Captured {
            succeeded: true,
            stdout: b"{\"kind\":\"test\"}".to_vec(),
            stderr: b"warning: ignored".to_vec(),
        });

        let text = interpret(captured, "cargo nextest list", Utf8Path::new("/w")).expect("a successful capture");

        assert_eq!(text, "{\"kind\":\"test\"}");
    }

    /// A command that never ran is reported, not read as having printed nothing.
    ///
    /// Proceeding on an empty capture would mean a workspace with no test binaries and a run that
    /// reports nothing to do — a perfect score for a suite that never executed.
    #[test]
    fn a_capture_that_could_not_be_spawned_is_reported() {
        let captured = Err(io::Error::new(io::ErrorKind::NotFound, "no such file"));
        let failure = interpret(captured, "cargo nextest list", Utf8Path::new("/w")).expect_err("a spawn failure must be reported");

        assert!(
            failure.to_string().contains("could not run `cargo nextest list` in /w"),
            "{failure}"
        );
        assert!(failure.to_string().contains("no such file"), "the cause is kept: {failure}");
    }

    /// A command that ran and failed is reported, with what it said about why.
    ///
    /// The standard error stream is the only place the reason exists — cargo prints its diagnostic
    /// there and exits non-zero — so dropping it leaves the user with a failure and no cause.
    #[test]
    fn a_capture_that_exited_non_zero_is_reported_with_its_diagnostic() {
        let captured: io::Result<Captured> = Ok(Captured {
            succeeded: false,
            stdout: b"partial output".to_vec(),
            stderr: b"  error: no such subcommand: `nextest`\n".to_vec(),
        });

        let failure = interpret(captured, "cargo nextest list", Utf8Path::new("/w")).expect_err("a non-zero exit must be reported");
        let text = failure.to_string();

        assert!(text.contains("`cargo nextest list` failed in /w"), "{text}");
        assert!(text.contains("error: no such subcommand: `nextest`"), "{text}");
        assert!(!text.contains("partial output"), "the failing output is not the answer: {text}");
    }

    /// Output that is not UTF-8 is reported rather than lossily salvaged.
    ///
    /// Every caller parses this text as JSON. Replacing an invalid sequence with `U+FFFD` would
    /// turn a corrupt capture into a parse error somewhere further away, or worse, into a
    /// successfully parsed document describing a path that is not the path on disk.
    #[test]
    fn a_capture_that_is_not_utf8_is_reported() {
        let captured: io::Result<Captured> = Ok(Captured {
            succeeded: true,
            stdout: vec![0x7b, 0xff, 0xfe, 0x7d],
            stderr: Vec::new(),
        });

        let failure = interpret(captured, "cargo metadata", Utf8Path::new("/w")).expect_err("invalid UTF-8 must be reported");

        assert!(
            failure.to_string().contains("`cargo metadata` did not print valid UTF-8"),
            "{failure}"
        );
    }

    #[test]
    fn scratch_files_create_parents_and_nextest_deduplicates_packages() {
        let directory = crate::testing::workdir("workspace-scratch-write-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("tree")).expect("UTF-8 root");
        let target = Utf8PathBuf::from_path_buf(directory.path().join("target")).expect("UTF-8 target");
        fs::create_dir_all(&root).expect("root");
        let work = Workspace::adopt(root, target.clone());

        let written = work.write_scratch("nested/data.json", "{}").expect("scratch file");
        assert_eq!(written, target.join("nested/data.json"));
        assert_eq!(fs::read_to_string(&written).expect("scratch contents"), "{}");

        let binaries = [
            TestBinary {
                package: "fallback".to_owned(),
                package_id: String::new(),
                ..crate::testing::test_binary("/first")
            },
            TestBinary {
                package: "fallback".to_owned(),
                package_id: String::new(),
                ..crate::testing::test_binary("/second")
            },
            TestBinary {
                package: "ignored".to_owned(),
                package_id: "precise-id".to_owned(),
                ..crate::testing::test_binary("/third")
            },
        ];
        let command = work.nextest_list_command(&binaries);
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(args.iter().filter(|arg| *arg == "fallback").count(), 1);
        assert_eq!(args.iter().filter(|arg| *arg == "precise-id").count(), 1);
    }

    #[test]
    fn testing_resume_restores_launch_environment_arguments_and_idle_resource_admission() {
        let directory = crate::testing::private_system_tempdir("workspace-testing-restore-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).unwrap();
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).unwrap();
        let root = base.join("workspace");
        create_private_dir_all(&source).unwrap();
        create_private_dir_all(&root).unwrap();
        fs::write(base.join("testing-verdicts.jsonl"), "").unwrap();
        mark_cache_owned_for_test(&source, &base);
        let mut original = Workspace::adopt(root, base.join("target"));
        original.runtime = base.join("rt");
        original.cargo.profile = Some("gamma".to_owned());
        original.cargo.test_args = vec!["--skip".into(), "slow".into()];
        original.test_environment = Some(TestEnvironment::fake("subject", "BUILD_SCRIPT_VALUE", "captured"));
        original.harness_threads = OnceLock::from(Some("3".into()));
        original.launch = OnceLock::from(Launch {
            loader: Some("captured-library-path".into()),
            stack: "7654321".into(),
        });
        let binary = TestBinary {
            package_id: "subject".into(),
            ..crate::testing::test_binary("a-test")
        };
        original.resources = Resources::fake_tests(&binary, &[("one", "database")]);
        let _held = original.acquire_resources(&binary, super::super::verdict::Only::One("one"));
        let snapshot = original.testing_snapshot().unwrap();
        assert!(snapshot["resources"].get("active").is_none());
        let restored = Workspace::resume_testing(
            &source,
            &base,
            snapshot.clone(),
            tempfile::tempfile().unwrap(),
            0,
            TestingResumeIo::FILESYSTEM,
        )
        .unwrap();
        assert_eq!(restored.testing_snapshot().unwrap(), snapshot);
        assert_eq!(restored.reporting_profile(), Some("gamma"));
        assert_eq!(restored.test_arguments(), ["--skip", "slow"]);
        assert_eq!(restored.harness_threads(), Some("3"));
        assert_eq!(restored.launch().stack, "7654321");
        let required = restored.required_resources(&binary, super::super::verdict::Only::One("one"));
        assert_eq!(required.iter().map(AsRef::as_ref).collect::<Vec<&str>>(), ["database"]);
        let mut command = Command::new("unused");
        restored.configure_test_environment(&mut command, &binary);
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "BUILD_SCRIPT_VALUE" && value == Some(OsStr::new("captured")))
        );
        let _fresh = restored.acquire_resources(&binary, super::super::verdict::Only::One("one"));
    }

    #[test]
    fn resume_validates_each_independent_workspace_shape_before_repairing_the_exact_journal_prefix() {
        let directory = crate::testing::private_system_tempdir("workspace-resume-shapes-");
        let directory = Utf8PathBuf::from_path_buf(fs::canonicalize(directory.path()).unwrap()).unwrap();
        let source = directory.join("source");
        let base = directory.join("cache");
        create_private_dir_all(&source).unwrap();
        create_private_dir_all(&base.join("workspace")).unwrap();
        mark_cache_owned_for_test(&source, &base);
        let journal = base.join("testing-verdicts.jsonl");
        fs::write(&journal, "prefix\nunfinished").unwrap();
        let mut original = Workspace::adopt(base.join("workspace"), base.join("target"));
        original.runtime = base.join("rt");
        original.launch = OnceLock::from(Launch {
            loader: Some("saved-loader".into()),
            stack: "7340032".into(),
        });
        let snapshot = original.testing_snapshot().unwrap();
        let foreign = directory.join("foreign");
        let resume = |source: &Utf8Path, snapshot, valid_bytes| {
            Workspace::resume_testing(
                source,
                &base,
                snapshot,
                tempfile::tempfile_in(&directory).unwrap(),
                valid_bytes,
                TestingResumeIo::FILESYSTEM,
            )
        };
        for (field, replacement) in [
            ("base", foreign.clone()),
            ("target", base.join("wrong-target")),
            ("root", base.join("wrong-root")),
            ("runtime", base.join("wrong-runtime")),
            ("root", foreign.join("workspace")),
        ] {
            let mut invalid = snapshot.clone();
            invalid[field] = to_value(replacement).unwrap();
            let failure = resume(&source, invalid, 7).unwrap_err();
            assert!(failure.message().starts_with("prepared testing workspace does not belong to"));
            assert_eq!(fs::read(&journal).unwrap(), b"prefix\nunfinished");
        }
        let failure = resume(&source, json!({}), 7).expect_err("an empty object is not a prepared workspace snapshot");
        assert_eq!(failure.message(), "prepared testing workspace is invalid; start a new campaign");
        let mut no_parent = snapshot.clone();
        no_parent["root"] = json!("");
        let failure = resume(&source, no_parent, 7).expect_err("a snapshot with no scratch parent is a returned validation error");
        assert_eq!(failure.message(), "prepared testing workspace has no scratch directory");
        for default_scratch in [false, true] {
            fs::write(&journal, "prefix\nunfinished").unwrap();
            let mut saved = snapshot.clone();
            if default_scratch {
                let scratch = gamma_base(&source, None);
                saved["root"] = to_value(scratch.join("workspace")).unwrap();
                saved["runtime"] = to_value(scratch.join("rt")).unwrap();
            }
            let mut restored = resume(&source, saved.clone(), 7).unwrap();
            assert_eq!(restored.testing_snapshot().unwrap(), saved);
            assert_eq!(fs::read(&journal).unwrap(), b"prefix\n");
            assert!(restored.testing_journal.is_some());
            #[expect(
                clippy::used_underscore_binding,
                reason = "the test verifies ownership of the retention-only cache lock"
            )]
            let holds_lock = restored._cache_lock.is_some();
            assert_eq!(holds_lock, !default_scratch);
            assert!(restored.settled.load(Ordering::Relaxed));
            assert!(!restored.leak);
            assert!(!restored.torn_down);
            assert!(!restored.rustc_captures_ready.load(Ordering::Relaxed));
            assert!(restored.rustc_captures.is_none());
            assert!(restored.cargo_metadata.is_none());
            restored.leak = true;
        }
        fs::remove_file(&journal).unwrap();
        let failure = resume(&source, snapshot.clone(), 7).expect_err("resuming requires the committed verdict journal");
        assert!(failure.message().contains("testing-verdicts.jsonl"), "{failure}");
        fs::write(&journal, "foreign-prefix\n").unwrap();
        let foreign_source = directory.join("foreign-source");
        fs::create_dir(&foreign_source).unwrap();
        let failure = resume(&foreign_source, snapshot, 0).expect_err("a foreign source cannot claim an owned cache");
        assert_eq!(
            failure.message(),
            format!(
                "the cargo-gamma cache at `{base}` belongs to the workspace at `{}`, not `{}`.\n{}",
                crate::report::encode_controls(physical(&source).as_str()),
                physical(&foreign_source),
                CacheKind::Redirected.collision_advice(),
            ),
            "{failure}"
        );
        assert_eq!(fs::read(&journal).unwrap(), b"foreign-prefix\n", "{failure}");
        assert_eq!(
            fs::read_to_string(base.join(CACHE_OWNER)).unwrap(),
            physical(&source).as_str(),
            "{failure}"
        );
    }

    #[test]
    fn resuming_the_default_all_in_one_cache_retains_only_its_existing_workspace_lock() {
        let directory = crate::testing::workdir("workspace-parent-default-resume-");
        let source = absolute(Utf8Path::from_path(directory.path()).unwrap());
        let base = gamma_base(&source, None);
        let lock = claim_workspace(&source).unwrap();
        fs::create_dir_all(base.join("workspace")).unwrap();
        fs::write(base.join("testing-verdicts.jsonl"), "").unwrap();
        let mut original = Workspace::adopt(base.join("workspace"), base.join("target"));
        original.runtime = base.join("rt");
        let snapshot = original.testing_snapshot().unwrap();
        let mut restored = Workspace::resume_testing(&source, &base, snapshot.clone(), lock, 0, TestingResumeIo::FILESYSTEM).unwrap();
        assert_eq!(restored.testing_snapshot().unwrap(), snapshot);
        #[expect(
            clippy::used_underscore_binding,
            reason = "verifying that resumption does not acquire its own lock twice"
        )]
        let has_extra_lock = restored._cache_lock.is_some();
        assert!(!has_extra_lock);
        restored.leak = true;
    }

    #[test]
    fn arming_checkpoints_and_completion_change_only_the_armed_campaign() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let base = Utf8PathBuf::from_path_buf(fs::canonicalize(directory.path()).unwrap()).unwrap();
        fs::create_dir(base.join("workspace")).unwrap();
        let mut work = Workspace::adopt(base.join("workspace"), base.join("target"));
        let failure = work
            .arm_testing_journal()
            .expect_err("a workspace cannot arm a missing verdict journal");
        assert!(failure.message().contains("testing-verdicts.jsonl"));
        fs::write(base.join("testing-session.json"), "{}").unwrap();
        let journal = base.join("testing-verdicts.jsonl");
        fs::write(&journal, "").unwrap();
        work.arm_testing_journal().unwrap();
        assert!(work.testing_journal.is_some());
        let mut mutant = crate::fixtures::mutant();
        mutant.outcome = crate::model::Outcome::Survived;
        let cost = super::super::session::SweepCost::default();
        work.checkpoint_mutant(&mutant, &super::super::killers::Killers::default(), &cost)
            .unwrap();
        let committed = fs::read_to_string(&journal).unwrap();
        assert_eq!(committed.lines().count(), 1);
        assert!(committed.ends_with('\n'));
        work.complete_testing().unwrap();
        let phase = base.join("testing-phase.json");
        let status: Value = from_slice(&fs::read(&phase).unwrap()).unwrap();
        assert_eq!(status["phase"], "FinishedTesting");
        work.testing_journal = None;
        fs::remove_file(&phase).unwrap();
        work.complete_testing().unwrap();
        assert!(!phase.exists());
        assert_eq!(fs::read_to_string(&journal).unwrap(), committed);
        let committed: Value = from_str(&committed).unwrap();
        let payload = format!(
            "{{\"mutant\":{},\"probe\":null,\"sweep\":{}}}",
            to_string(&mutant).unwrap(),
            to_string(&cost).unwrap()
        );
        assert_eq!(
            committed,
            json!({
                "mutant": mutant,
                "probe": null,
                "sweep": cost,
                "checksum": blake3::hash(payload.as_bytes()).to_hex().to_string(),
            })
        );
    }

    #[test]
    fn cargo_commands_preserve_the_workspace_environment_and_selection() {
        let directory = crate::testing::workdir("workspace-command-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("root")).expect("UTF-8 root");
        let target = Utf8PathBuf::from_path_buf(directory.path().join("target")).expect("UTF-8 target");
        fs::create_dir_all(&root).expect("root");
        let mut work = Workspace::adopt(root.clone(), target.clone());
        work.cargo.color = true;

        let command = work.cargo();
        assert_eq!(command.get_current_dir(), Some(root.as_std_path()));
        let env: BTreeMap<_, _> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(env["CARGO_TARGET_DIR"].as_deref(), Some(target.as_str()));
        assert_eq!(env["CARGO_TERM_COLOR"].as_deref(), Some("always"));
        assert!(env.contains_key(gamma_rt::ACTIVE_VAR));
        assert_eq!(env[gamma_rt::ACTIVE_VAR], None);

        work.cargo.color = false;
        let command = work.cargo();
        let color = command
            .get_envs()
            .find(|(name, _value)| *name == "CARGO_TERM_COLOR")
            .and_then(|(_name, value)| value);
        assert_eq!(color, Some(OsStr::new("never")));
    }

    #[test]
    fn build_sequence_retains_rustc_captures_across_cargo_commands() {
        let directory = crate::testing::workdir("workspace-captures-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("root")).expect("UTF-8 root");
        let target = Utf8PathBuf::from_path_buf(directory.path().join("target")).expect("UTF-8 target");
        fs::create_dir_all(&root).expect("root");
        let mut work = Workspace::adopt(root, target.clone());
        let captures = target.join(".cargo-gamma-rustc");
        work.rustc_captures = Some(captures.clone());

        fs::create_dir_all(&captures).expect("capture directory");
        fs::write(captures.join("stale.json"), b"stale").expect("stale capture");
        work.begin_build_sequence();
        assert!(!captures.join("stale.json").exists(), "a new sequence clears old captures");

        fs::write(captures.join("preflight.json"), b"preflight").expect("preflight capture");
        drop(work.cargo());
        fs::write(captures.join("stage.json"), b"stage").expect("stage capture");
        drop(work.cargo());

        assert!(
            captures.join("preflight.json").exists(),
            "later cargo commands retain preflight captures"
        );
        assert!(captures.join("stage.json").exists(), "later cargo commands retain staged captures");
    }

    #[test]
    fn a_self_referential_outer_rustc_wrapper_is_not_chained() {
        let directory = crate::testing::workdir("workspace-wrapper-self-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("root")).expect("UTF-8 root");
        let wrapper = root.join(format!("cargo-gamma{}", std::env::consts::EXE_SUFFIX));
        let other = root.join(format!("sccache{}", std::env::consts::EXE_SUFFIX));
        fs::create_dir_all(&root).expect("root");
        fs::write(&wrapper, b"wrapper").expect("wrapper");
        fs::write(&other, b"other wrapper").expect("other wrapper");

        assert!(wrapper_is_self(OsStr::new("cargo-gamma"), &wrapper, &root));
        assert!(wrapper_is_self(wrapper.as_os_str(), &wrapper, &root));
        assert!(!wrapper_is_self(other.as_os_str(), &wrapper, &root));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_alias_to_this_rustc_wrapper_is_not_chained() {
        let directory = crate::testing::workdir("workspace-wrapper-non-utf8-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("root")).expect("UTF-8 root");
        let wrapper = root.join("cargo-gamma");
        fs::create_dir_all(&root).expect("root");
        fs::write(&wrapper, b"wrapper").expect("wrapper");

        let alias = OsString::from_vec(b"wrapper-\xff".to_vec());
        std::os::unix::fs::symlink(wrapper.as_std_path(), root.as_std_path().join(&alias)).expect("non-UTF-8 wrapper alias");

        assert!(wrapper_is_self(&alias, &wrapper, &root));
    }

    #[test]
    fn workspace_lifecycle_removes_only_unsettled_unleaked_trees() {
        let directory = crate::testing::workdir("workspace-lifecycle-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("root")).expect("UTF-8 root");
        let target = Utf8PathBuf::from_path_buf(directory.path().join("target")).expect("UTF-8 target");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&target).expect("target");

        let mut work = Workspace::adopt(root.clone(), target.clone());
        work.leak = false;
        work.settled.store(false, Ordering::Relaxed);
        work.teardown().expect("teardown");
        assert!(work.torn_down);
        assert!(!root.exists());
        assert!(!target.exists());
        work.teardown().expect("a second teardown is inert");

        let kept_root = Utf8PathBuf::from_path_buf(directory.path().join("kept-root")).expect("UTF-8 root");
        let kept_target = Utf8PathBuf::from_path_buf(directory.path().join("kept-target")).expect("UTF-8 target");
        fs::create_dir_all(&kept_root).expect("root");
        fs::create_dir_all(&kept_target).expect("target");
        let mut kept = Workspace::adopt(kept_root.clone(), kept_target.clone());
        kept.leak = false;
        kept.teardown().expect("settled teardown");
        assert!(kept_root.exists());
        assert!(kept_target.exists());
    }

    #[test]
    fn harness_calibration_is_stable_after_its_first_decision() {
        let directory = crate::testing::workdir("workspace-harness-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 root");
        let work = Workspace::adopt(root.clone(), root.join("target"));

        work.calibrate_harness(1);
        let first = work.harness_threads().map(str::to_owned);
        work.calibrate_harness(usize::MAX);
        assert_eq!(work.harness_threads(), first.as_deref());
    }

    #[test]
    fn cargo_metadata_capture_is_nonempty_json_from_the_adopted_root() {
        let directory = crate::testing::workdir("workspace-metadata-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 root");
        crate::testing::write_project(
            root.as_std_path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"metadata-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n",
                ),
                ("src/lib.rs", ""),
            ],
        );
        let work = Workspace::adopt(root.clone(), root.join("target"));

        let metadata = work.capture_cargo_metadata().expect("cargo metadata");
        let parsed: Value = from_str(&metadata).expect("metadata JSON");
        assert_eq!(parsed["packages"][0]["name"], "metadata-fixture");
        let metadata_root = parsed["workspace_root"]
            .as_str()
            .expect("cargo metadata always reports a string workspace root");
        assert_eq!(physical(Utf8Path::new(metadata_root)), physical(&root));
    }

    #[test]
    fn overwrite_requires_a_real_file_inside_the_scratch_root_and_avoids_rewrites() {
        let directory = crate::testing::workdir("workspace-overwrite-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("root")).expect("UTF-8 root");
        let file = root.join("src/lib.rs");
        fs::create_dir_all(file.parent().expect("parent")).expect("parent");
        fs::write(&file, "old").expect("file");

        assert!(!Workspace::overwrite(&root, &file, "old").expect("identical contents"));
        assert!(Workspace::overwrite(&root, &file, "new").expect("changed contents"));
        assert_eq!(fs::read_to_string(&file).expect("contents"), "new");

        let directory_path = root.join("src");
        let error = Workspace::overwrite(&root, &directory_path, "no").expect_err("a directory is not source");
        assert!(error.to_string().contains("link or a device"), "{error}");

        let absent = root.join("missing.rs");
        let error = Workspace::overwrite(&root, &absent, "no").expect_err("an absent file was not copied");
        assert!(error.to_string().contains("did not create"), "{error}");
    }

    #[test]
    fn manifest_lookup_stops_at_the_scratch_root_and_never_uses_an_outer_manifest() {
        let directory = crate::testing::workdir("workspace-manifest-lookup-");
        let outer = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 root");
        fs::write(outer.join("Cargo.toml"), "[workspace]\n").expect("outer manifest");
        let root = outer.join("scratch");
        fs::create_dir_all(root.join("crate/src")).expect("source");
        fs::write(root.join("crate/Cargo.toml"), "[package]\nname=\"inside\"\nversion=\"0.0.0\"\n").expect("inner manifest");
        let work = Workspace::adopt(root.clone(), outer.join("target"));
        let files = [TargetFile {
            path: Utf8PathBuf::from("crate/src/lib.rs"),
            absolute: root.join("crate/src/lib.rs"),
            package: "inside".to_owned(),
            source: None,
        }];

        assert_eq!(work.manifest_of("inside", &files), Some(root.join("crate/Cargo.toml")));
        assert_eq!(work.manifest_of("absent", &files), None);

        fs::remove_file(root.join("crate/Cargo.toml")).expect("inner manifest");
        assert_eq!(work.manifest_of("inside", &files), None);
    }

    fn source_snapshot_plan(root: &Utf8Path, paths: &[&str]) -> crate::discover::Plan {
        crate::discover::Plan {
            root: root.to_owned(),
            files: paths
                .iter()
                .map(|path| TargetFile {
                    path: (*path).into(),
                    absolute: root.join(path),
                    package: "subject".into(),
                    source: Some(format!("source of {path}")),
                })
                .collect(),
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            skipped: Vec::new(),
            digests: HashMap::default(),
            reach: HashMap::default(),
            specs: HashMap::default(),
        }
    }

    #[test]
    fn report_snapshot_failures_identify_the_exact_filesystem_operation() {
        for (paths, operation) in [
            (vec!["blocked", "blocked/child.rs"], "could not create report source directory"),
            (vec!["directory/child.rs", "directory"], "could not retain report source"),
        ] {
            let temporary = tempfile::tempdir_in(".").unwrap();
            let base = Utf8PathBuf::from_path_buf(temporary.path().canonicalize().unwrap()).unwrap();
            let work = Workspace::adopt(base.join("workspace"), base.join("target"));
            let failure = work
                .snapshot_report_sources(&source_snapshot_plan(work.root(), &paths))
                .expect_err("the earlier retained path collides with the later path");
            assert!(failure.message().starts_with(operation), "{failure}");
            let first = work.report_source_root().join(paths[0]);
            assert_eq!(fs::read_to_string(first).unwrap(), format!("source of {}", paths[0]));
        }
        let temporary = tempfile::tempdir_in(".").unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().canonicalize().unwrap()).unwrap();
        let mut work = Workspace::adopt(base.join("workspace"), base.join("target"));
        work.base = base.join("missing-parent");
        let failure = work
            .snapshot_report_sources(&source_snapshot_plan(work.root(), &[]))
            .expect_err("the sidecar's parent does not exist");
        assert_eq!(
            failure.message(),
            format!("could not create report source directory `{}`", work.report_source_root())
        );
        work.base = base.join("\0invalid-parent");
        let failure = work
            .snapshot_report_sources(&source_snapshot_plan(work.root(), &[]))
            .expect_err("NUL is invalid in a filesystem path");
        assert_eq!(
            failure.message(),
            format!("could not inspect report source directory `{}`", work.report_source_root())
        );
    }

    #[test]
    fn scratch_collisions_and_outside_source_writes_return_context_without_mutating_files() {
        let temporary = tempfile::tempdir_in(".").unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().canonicalize().unwrap()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&target).unwrap();
        let work = Workspace::adopt(root.clone(), target.clone());
        fs::write(target.join("blocked"), "parent file").unwrap();
        let failure = work
            .write_scratch("blocked/child", "new")
            .expect_err("a regular file cannot become the scratch parent");
        assert_eq!(failure.message(), format!("could not create `{}`", target.join("blocked")));
        fs::create_dir(target.join("directory")).unwrap();
        let failure = work
            .write_scratch("directory", "new")
            .expect_err("a scratch write cannot replace a directory");
        assert_eq!(failure.message(), format!("could not write `{}`", target.join("directory")));
        let outside = base.join("user.rs");
        fs::write(&outside, "untouched user source").unwrap();
        let failure = Workspace::overwrite(&root, &outside, "mutated source").expect_err("the regular file is outside the copied root");
        assert_eq!(
            failure.message(),
            format!(
                "refusing to write `{outside}`, which is `{}` — outside the scratch tree at `{}`, so the write would land in the real source tree",
                physical(&outside),
                physical(&root)
            )
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched user source");
        assert_eq!(fs::read_to_string(target.join("blocked")).unwrap(), "parent file");
        assert!(target.join("directory").is_dir());
    }

    #[test]
    fn a_linked_report_sidecar_is_refused_without_clearing_its_target() {
        let temporary = tempfile::tempdir_in(".").unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().canonicalize().unwrap()).unwrap();
        let work = Workspace::adopt(base.join("workspace"), base.join("target"));
        let outside = base.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "user bytes").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, work.report_source_root()).unwrap();
        #[cfg(windows)]
        {
            let output = Command::new("cmd.exe")
                .args(["/c", "mklink", "/J"])
                .arg(work.report_source_root().as_str().replace('/', "\\"))
                .arg(outside.as_str().replace('/', "\\"))
                .output()
                .unwrap();
            assert!(output.status.success(), "junction creation failed: {output:?}");
        }
        let failure = work
            .snapshot_report_sources(&source_snapshot_plan(work.root(), &[]))
            .expect_err("a sidecar link is not owned report storage");
        assert_eq!(
            failure.message(),
            format!("report source directory `{}` is a link", work.report_source_root())
        );
        assert_eq!(fs::read_to_string(outside.join("sentinel")).unwrap(), "user bytes");
        assert!(fs::symlink_metadata(work.report_source_root()).unwrap().file_type().is_symlink());
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_report_snapshot_returns_the_clear_error_without_discarding_the_old_source() {
        let temporary = tempfile::tempdir_in(".").unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().canonicalize().unwrap()).unwrap();
        let work = Workspace::adopt(base.join("workspace"), base.join("target"));
        fs::create_dir(work.report_source_root()).unwrap();
        let source = work.report_source_root().join("locked.rs");
        fs::write(&source, "old source").unwrap();
        let lock = fs::OpenOptions::new().read(true).share_mode(0).open(&source).unwrap();
        let failure = work
            .snapshot_report_sources(&source_snapshot_plan(work.root(), &[]))
            .expect_err("the exclusive handle denies clearing the prior sidecar");
        assert_eq!(
            failure.message(),
            format!("could not clear report source directory `{}`", work.report_source_root())
        );
        drop(lock);
        assert_eq!(fs::read_to_string(&source).unwrap(), "old source");
    }

    #[test]
    fn teardown_is_idempotent_and_respects_both_preservation_flags_independently() {
        for settled in [false, true] {
            for leak in [false, true] {
                let temporary = tempfile::tempdir_in(".").unwrap();
                let base = Utf8PathBuf::from_path_buf(temporary.path().canonicalize().unwrap()).unwrap();
                let root = base.join("workspace");
                let target = base.join("target");
                fs::create_dir(&root).unwrap();
                fs::create_dir(&target).unwrap();
                fs::write(root.join("source.rs"), "source").unwrap();
                fs::write(target.join("artifact"), "artifact").unwrap();
                let mut work = unsettled(&base, &root, &target);
                work.settled.store(settled, Ordering::Relaxed);
                work.leak = leak;
                work.teardown().unwrap();
                assert!(work.torn_down);
                assert_eq!(root.exists(), settled || leak);
                assert_eq!(target.exists(), settled || leak);
                fs::create_dir_all(&root).unwrap();
                fs::create_dir_all(&target).unwrap();
                fs::write(root.join("new-source.rs"), "later source").unwrap();
                fs::write(target.join("new-artifact"), "later artifact").unwrap();
                work.settled.store(false, Ordering::Relaxed);
                work.leak = false;
                work.teardown().unwrap();
                drop(work);
                assert_eq!(fs::read_to_string(root.join("new-source.rs")).unwrap(), "later source");
                assert_eq!(fs::read_to_string(target.join("new-artifact")).unwrap(), "later artifact");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn denied_writes_preserve_copied_sources_and_scratch_files() {
        let temporary = tempfile::tempdir_in(".").unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().canonicalize().unwrap()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&target).unwrap();
        let work = Workspace::adopt(root.clone(), target.clone());
        for scratch in [false, true] {
            let path = if scratch { target.join("locked") } else { root.join("locked") };
            fs::write(&path, "original bytes").unwrap();
            let lock = fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
            let failure = if scratch {
                work.write_scratch("locked", "replacement")
                    .expect_err("the exclusive handle denies reopening the scratch file")
            } else {
                Workspace::overwrite(&root, &path, "replacement").expect_err("the exclusive handle denies reopening the copied file")
            };
            assert_eq!(failure.message(), format!("could not write `{path}`"));
            drop(lock);
            assert_eq!(fs::read_to_string(&path).unwrap(), "original bytes");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_failed_tree_removal_still_cleans_the_other_tree_and_is_not_retried_on_drop() {
        let temporary = tempfile::tempdir_in(".").unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().canonicalize().unwrap()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&target).unwrap();
        let path = root.join("locked");
        fs::write(&path, "source").unwrap();
        let lock = fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
        let mut work = unsettled(&base, &root, &target);
        let failure = work.teardown().expect_err("the exclusive handle prevents removing the copied tree");
        assert!(failure.message().contains(&root.to_string()), "{failure}");
        assert!(work.torn_down);
        assert!(!target.exists());
        drop(lock);
        drop(work);
        assert_eq!(fs::read_to_string(&path).unwrap(), "source");
    }

    #[test]
    fn report_sources_use_a_fresh_sidecar_outside_the_copied_workspace() {
        let temporary = tempfile::tempdir().expect("temporary cache");
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_path_buf()).expect("UTF-8 cache");
        let root = base.join("workspace");
        let target = base.join("target");
        fs::create_dir_all(root.join(".cargo-gamma-report-source")).expect("copied collision");
        fs::write(root.join(".cargo-gamma-report-source/sentinel"), "workspace").expect("workspace sentinel");
        fs::create_dir_all(base.join(".cargo-gamma-report-source")).expect("old sidecar");
        fs::write(base.join(".cargo-gamma-report-source/stale"), "old").expect("stale sidecar");
        fs::create_dir_all(&target).expect("target");
        let work = unsettled(&base, &root, &target);
        let plan = crate::discover::Plan {
            root: root.clone(),
            files: vec![
                TargetFile {
                    path: "src/not-retained.rs".into(),
                    absolute: root.join("src/not-retained.rs"),
                    package: "subject".into(),
                    source: None,
                },
                TargetFile {
                    path: Utf8PathBuf::from("src/lib.rs"),
                    absolute: root.join("src/lib.rs"),
                    package: "subject".to_owned(),
                    source: Some("pub fn answer() -> i32 { 42 }\n".to_owned()),
                },
            ],
            mutants: Vec::new(),
            suppressed: 0,
            idle: Vec::new(),
            sharded_out: 0,
            settled_out: 0,
            skipped: Vec::new(),
            digests: HashMap::default(),
            reach: HashMap::default(),
            specs: HashMap::default(),
        };

        work.snapshot_report_sources(&plan).expect("source snapshot");

        assert_eq!(
            fs::read_to_string(base.join(".cargo-gamma-report-source/src/lib.rs")).expect("retained source"),
            "pub fn answer() -> i32 { 42 }\n"
        );
        assert!(!base.join(".cargo-gamma-report-source/stale").exists());
        assert!(!base.join(".cargo-gamma-report-source/src/not-retained.rs").exists());
        assert_eq!(
            fs::read_to_string(root.join(".cargo-gamma-report-source/sentinel")).expect("copied collision remains untouched"),
            "workspace"
        );
        fs::remove_dir_all(work.report_source_root()).unwrap();
        fs::write(work.report_source_root(), "do not overwrite this regular file").unwrap();
        let failure = work.snapshot_report_sources(&plan).unwrap_err();
        assert_eq!(
            failure.message(),
            format!("report source path `{}` is not a directory", work.report_source_root())
        );
        assert_eq!(
            fs::read_to_string(work.report_source_root()).unwrap(),
            "do not overwrite this regular file"
        );
    }

    #[test]
    fn cache_home_fallbacks_follow_cargo_precedence() {
        let root = absolute(Utf8Path::new("workspace"));
        let local = root.join("local");
        let home = root.join("home");

        let from_local = default_cache_home_with(&root, |name| (name == "LOCALAPPDATA").then(|| OsString::from(local.as_str())));
        assert_eq!(from_local, local.join("cargo-gamma"));

        let from_home = default_cache_home_with(&root, |name| (name == "HOME").then(|| OsString::from(home.as_str())));
        assert_eq!(from_home, home.join(".cache/cargo-gamma"));

        let fallback = default_cache_home_with(&root, |_name| None);
        assert_eq!(
            fallback,
            root.parent()
                .expect("absolute workspace has a parent")
                .join(".cargo-gamma-cache/cargo-gamma")
        );
    }

    #[test]
    fn physical_paths_preserve_a_completely_unresolvable_spelling() {
        let path = Utf8Path::new("");
        assert_eq!(physical(path), path);
    }

    #[test]
    fn copy_pruning_predicates_distinguish_lexical_relationships() {
        let source = Utf8Path::new("workspace");
        let inside = Utf8Path::new("workspace/cache");
        let outside = Utf8Path::new("cache");

        assert!(prunes(source, inside));
        assert!(!prunes(source, source));
        assert!(!prunes(source, outside));
        assert!(prunes_in_practice(source, inside, source, inside));
        assert!(!prunes_in_practice(source, outside, source, outside));
    }

    #[test]
    fn a_workspace_without_vcs_metadata_can_be_relocated() {
        let directory = crate::testing::workdir("workspace-no-vcs-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("UTF-8 source");
        let scratch = Utf8PathBuf::from_path_buf(directory.path().join("scratch")).expect("UTF-8 scratch");
        fs::create_dir_all(&source).expect("source");

        ensure_vcs_visibility(&source, &scratch).expect("there is no metadata to hide");
    }

    #[test]
    fn git_directories_and_pointer_files_are_exposed_to_the_scratch_tree() {
        let directory = crate::testing::workdir("workspace-expose-vcs-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 root");
        let source = root.join("source");
        let scratch = source.join("scratch");
        let git = source.join(".git");
        fs::create_dir_all(&git).expect("git directory");
        fs::create_dir_all(&scratch).expect("scratch");

        expose_vcs_metadata(&source, &scratch).expect("directory metadata");
        assert_eq!(
            fs::read_to_string(scratch.join(".git")).expect("git pointer"),
            format!("gitdir: {}\n", absolute(&git))
        );

        fs::remove_file(scratch.join(".git")).expect("first pointer");
        fs::remove_dir_all(&git).expect("git directory");
        let actual_git = root.join("actual-git");
        fs::create_dir_all(&actual_git).expect("actual git");
        fs::write(&git, "gitdir: ../actual-git\n").expect("git pointer source");

        expose_vcs_metadata(&source, &scratch).expect("pointer metadata");
        assert_eq!(
            fs::read_to_string(scratch.join(".git")).expect("rewritten pointer"),
            format!("gitdir: {}\n", absolute(&actual_git))
        );
    }

    fn initialize_git_build_fixture(source: &Utf8Path, target: &Utf8Path) -> String {
        crate::testing::write_project(
            source.as_std_path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"g\"\nversion = \"0.0.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\n[workspace]\n",
                ),
                (".cargo/config.toml", &format!("[build]\ntarget-dir = '{}'\n", target.as_str())),
                ("src/lib.rs", "pub fn present() {}\n"),
                (
                    "build.rs",
                    r#"fn main() {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git rev-parse failed");
    std::fs::write("observed-head", output.stdout).expect("head is recorded");
}
"#,
                ),
            ],
        );

        let run_git = |arguments: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(source)
                .args(arguments)
                .output()
                .expect("git runs");
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            output
        };
        let _ = run_git(&["init", "--quiet"]);
        let _ = run_git(&["add", "."]);
        let _ = run_git(&[
            "-c",
            "user.name=cargo-gamma test",
            "-c",
            "user.email=cargo-gamma@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ]);

        String::from_utf8(run_git(&["rev-parse", "HEAD"]).stdout)
            .expect("Git hashes are UTF-8")
            .trim()
            .to_owned()
    }

    fn assert_external_target_preserves_git_context(source: &Utf8Path, target: &Utf8Path, expected_head: &str) {
        let survey = crate::discover::Survey::for_build_using(
            &crate::commands::SelectArgs {
                dir: source.to_owned(),
                ..crate::commands::SelectArgs::default()
            },
            None,
            &crate::exec::CargoOptions::default(),
            |command| {
                let mut command = command.clone();
                command.env_remove("CARGO_TARGET_DIR").exec()
            },
        )
        .expect("Cargo metadata resolves the configured external target");
        assert_eq!(survey.target, target);

        let mut events = crate::testing::Recorder::default();
        let work = Workspace::prepare(&survey.root, &survey.target, &Config::default(), &mut events)
            .expect("the production preparation path supports an external Cargo target");

        let output = work.cargo().arg("build").arg("--quiet").output().expect("cargo runs");

        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(
            fs::read_to_string(work.root.join("observed-head"))
                .expect("the build script recorded the selected commit")
                .trim(),
            expected_head
        );
        assert!(
            fs::read_dir(work.target.as_std_path())
                .expect("campaign target directory")
                .next()
                .is_some(),
            "Cargo artifacts must be created beneath the campaign cache rooted at the external target"
        );
        assert!(
            !work.root.join("target").exists(),
            "the synchronized workspace must not gain a local target"
        );
    }

    #[test]
    fn an_external_target_preserves_git_context_for_a_checkout_and_linked_worktree() {
        let directory = crate::testing::workdir("git-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 root");
        let repository = root.join("r");
        let target = root.join("t");
        let expected_head = initialize_git_build_fixture(&repository, &target);

        assert_external_target_preserves_git_context(&repository, &target, &expected_head);

        let worktree = root.join("w");
        let output = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["worktree", "add", "--quiet", "--detach"])
            .arg(&worktree)
            .arg("HEAD")
            .output()
            .expect("git worktree runs");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_external_target_preserves_git_context(&worktree, &target, &expected_head);
    }

    #[test]
    fn empty_and_non_utf8_owner_markers_are_rejected() {
        let directory = crate::testing::workdir("workspace-owner-invalid-");
        let base = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 base");
        let source = base.join("source");
        let owner = base.join(CACHE_OWNER);

        fs::write(&owner, "").expect("empty owner");
        let empty = validate_cache_owner(&source, &base, CacheKind::Redirected).expect_err("empty marker");
        assert!(empty.to_string().contains("invalid length"), "{empty}");

        fs::write(&owner, [0xff]).expect("non-UTF-8 owner");
        let invalid = validate_cache_owner(&source, &base, CacheKind::Redirected).expect_err("non-UTF-8 marker");
        assert!(invalid.to_string().contains("could not read"), "{invalid}");
    }

    #[test]
    fn a_cache_containing_only_its_lock_has_no_unowned_entries() {
        let directory = crate::testing::workdir("workspace-owned-lock-");
        let base = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("UTF-8 base");
        fs::write(base.join("lock"), "lock").expect("lock");

        assert!(!has_unowned_entries(&base).expect("cache scan"));
    }

    /// Exercises every step of preparing a scratch tree — creating the base, claiming the lock,
    /// copying, vendoring the runtime, and anchoring manifests.
    #[test]
    fn preparing_a_fresh_workspace_produces_a_usable_scratch_tree() {
        let directory = crate::testing::workdir("prepare-happy-");
        let source = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");

        crate::testing::write_project(
            source.as_std_path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"trivial\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
                ),
                ("src/lib.rs", "pub const A: i32 = 1;\n"),
            ],
        );

        let config = Config::default();
        let mut events = crate::testing::Recorder::default();
        let scratch = gamma_base(&source, None);
        let campaign = campaign_base(&source, &source.join("target"), None);
        let work = Workspace::prepare(&source, &source.join("target"), &config, &mut events).expect("a fresh tree must prepare cleanly");

        assert_eq!(work.root, scratch.join("workspace"));
        assert_eq!(work.runtime, scratch.join("rt"));
        assert_eq!(work.base(), campaign);
        assert_eq!(work.target, campaign.join("target"));
        assert!(work.root.join("Cargo.toml").as_std_path().is_file(), "the manifest was not copied");
        assert!(
            work.root.join("src/lib.rs").as_std_path().is_file(),
            "the source file was not copied"
        );
        assert!(
            work.runtime.join("Cargo.toml").as_std_path().is_file(),
            "the runtime was not vendored"
        );
    }

    #[test]
    fn an_unowned_redirected_cache_is_refused_without_touching_its_contents() {
        let directory = private_system_tempdir("unowned-cache-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the source path is UTF-8");
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).expect("the cache path is UTF-8");
        let marker = base.join("build/user-data");

        fs::create_dir_all(marker.parent().expect("the marker has a parent")).expect("the user's directory");
        fs::write(marker.as_std_path(), "keep").expect("the user's file");

        let failure = claim_redirected_cache(&source, &base).expect_err("an unowned non-empty directory must be refused");

        assert!(failure.is_usage());
        assert!(failure.to_string().contains("not marked as cargo-gamma state"), "{failure}");
        assert_eq!(fs::read_to_string(marker.as_std_path()).expect("the user's file remains"), "keep");
        assert!(
            !base.join("lock").exists(),
            "refusal must happen before cargo-gamma writes into the directory"
        );
        assert!(!base.join(CACHE_OWNER).exists());
    }

    #[test]
    fn a_directory_containing_only_somebody_elses_lock_file_is_not_adopted() {
        let directory = private_system_tempdir("foreign-lock-cache-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the source path is UTF-8");
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).expect("the cache path is UTF-8");

        create_private_dir_all(&base).expect("the user's directory");
        fs::write(base.join("lock"), "not ours").expect("the user's lock file");

        let failure = claim_redirected_cache(&source, &base).expect_err("an existing lock file is user data");

        assert!(failure.is_usage());
        assert_eq!(fs::read_to_string(base.join("lock")).expect("the user's lock remains"), "not ours");
        assert!(!base.join(CACHE_OWNER).exists());
    }

    #[test]
    fn an_empty_redirected_cache_is_claimed_for_its_workspace_and_can_be_reused() {
        let directory = private_system_tempdir("owned-cache-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the source path is UTF-8");
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).expect("the cache path is UTF-8");

        create_private_dir_all(&base).expect("the empty cache");
        let first = claim_redirected_cache(&source, &base).expect("an empty cache can be claimed");

        assert_eq!(
            fs::read_to_string(base.join(CACHE_OWNER)).expect("the owner marker"),
            physical(&source).as_str()
        );

        drop(first);
        let _second = claim_redirected_cache(&source, &base).expect("the owning workspace can reuse its cache");
    }

    #[test]
    fn a_redirected_cache_owned_by_another_workspace_is_refused() {
        let directory = private_system_tempdir("foreign-cache-");
        let first = Utf8PathBuf::from_path_buf(directory.path().join("first")).expect("the source path is UTF-8");
        let second = Utf8PathBuf::from_path_buf(directory.path().join("second")).expect("the source path is UTF-8");
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).expect("the cache path is UTF-8");

        let held = claim_redirected_cache(&first, &base).expect("the first workspace claims the cache");
        drop(held);

        let failure = claim_redirected_cache(&second, &base).expect_err("a second workspace must not adopt the cache");

        assert!(failure.is_usage());
        assert!(failure.to_string().contains("belongs to the workspace"), "{failure}");
        assert!(failure.to_string().contains(physical(&first).as_str()), "{failure}");
    }

    /// Cache creation must not inherit a build agent's permissive umask and then reject the
    /// directory cargo-gamma just created as unsafe.
    #[cfg(unix)]
    #[test]
    fn a_new_redirected_cache_is_private_under_a_permissive_umask() {
        use std::os::unix::fs::PermissionsExt as _;

        const CHILD: &str = "CARGO_GAMMA_PERMISSIVE_UMASK_CHILD";
        const TEST: &str = "exec::workspace::tests::a_new_redirected_cache_is_private_under_a_permissive_umask";

        if env::var_os(CHILD).is_none() {
            let executable = env::current_exe().expect("the current test executable");
            let output = Command::new("sh")
                .args(["-c", "umask 000; exec \"$1\" --exact \"$2\" --nocapture", "sh"])
                .arg(executable)
                .arg(TEST)
                .env(CHILD, "1")
                .output()
                .expect("the child test process starts");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            assert!(output.status.success(), "child stdout:\n{stdout}\nchild stderr:\n{stderr}");
            return;
        }
        let directory = private_system_tempdir("private-cache-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the source path is UTF-8");
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).expect("the cache path is UTF-8");

        let _held = claim_redirected_cache(&source, &base).expect("a new cache is private under any umask");
        let mode = fs::metadata(base.as_std_path()).expect("the cache metadata").permissions().mode();

        assert_eq!(mode & 0o077, 0, "the cache mode {mode:o} grants group or other access");
    }

    /// A cache reached through a link is a cache whose identity can be changed after it has been
    /// approved, so the name that was checked is not the directory that gets built into and run
    /// from.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_linked_redirected_cache_is_refused() {
        let directory = private_system_tempdir("linked-cache-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the source path is UTF-8");
        let real = Utf8PathBuf::from_path_buf(directory.path().join("real")).expect("the real path is UTF-8");
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).expect("the cache path is UTF-8");

        fs::create_dir_all(&real).expect("the directory the link points at");

        #[cfg(unix)]
        symlink(real.as_std_path(), base.as_std_path()).expect("the link");

        // Creating a link needs a privilege or developer mode on Windows, and a runner without it
        // cannot exercise this at all. Skipped rather than failed: the check being tested is the
        // same one either way, and a test that demands a privilege reports the runner's
        // configuration rather than this tool's behaviour.
        #[cfg(windows)]
        if symlink_dir(real.as_std_path(), base.as_std_path()).is_err() {
            return;
        }

        let failure = claim_redirected_cache(&source, &base).expect_err("a linked cache must be refused");

        assert!(failure.is_usage());
        assert!(failure.to_string().contains("is a link"), "{failure}");
        assert!(!real.join(CACHE_OWNER).exists(), "nothing may be written through the link");
    }

    /// A regular file where a cache directory was named is not a cache, and the marker checks would
    /// otherwise be asked about a directory that does not exist.
    #[test]
    fn a_redirected_cache_that_is_not_a_directory_is_refused() {
        let directory = private_system_tempdir("file-cache-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the source path is UTF-8");
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).expect("the cache path is UTF-8");

        fs::write(base.as_std_path(), "not a directory").expect("the user's file");

        let failure = claim_redirected_cache(&source, &base).expect_err("a file is not a cache directory");

        assert!(failure.is_usage());
        assert_eq!(
            fs::read_to_string(base.as_std_path()).expect("the user's file remains"),
            "not a directory"
        );
    }

    /// Everything a run builds is placed in the cache and then executed, so a directory anywhere
    /// above it that another local user can write to is a directory from which they choose what
    /// this tool runs as the invoking user.
    #[cfg(unix)]
    #[test]
    fn a_redirected_cache_under_a_world_writable_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = private_system_tempdir("shared-cache-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the source path is UTF-8");
        let shared = Utf8PathBuf::from_path_buf(directory.path().join("shared")).expect("the shared path is UTF-8");
        let base = shared.join("cache");

        create_private_dir_all(&base).expect("the cache directory");
        fs::set_permissions(shared.as_std_path(), fs::Permissions::from_mode(0o777)).expect("make the parent world-writable");

        let failure = claim_redirected_cache(&source, &base).expect_err("a world-writable ancestor must be refused");

        assert!(failure.is_usage());
        assert!(failure.to_string().contains("is writable by other users"), "{failure}");
        assert!(!base.join(CACHE_OWNER).exists());
        assert!(!base.join("lock").exists(), "refusal must happen before anything is written");

        // Restored so that the temporary directory can be removed by the harness that made it.
        fs::set_permissions(shared.as_std_path(), fs::Permissions::from_mode(0o755)).expect("restore the parent");
    }

    /// The sticky bit is what makes a shared directory safe to keep a private one inside: it stops
    /// one user removing or renaming another's entries, which is the substitution being defended
    /// against. Refusing it would rule out every cache under `/tmp` for no gain.
    #[cfg(unix)]
    #[test]
    fn a_redirected_cache_under_a_sticky_shared_directory_is_allowed() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = private_system_tempdir("sticky-cache-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the source path is UTF-8");
        let shared = Utf8PathBuf::from_path_buf(directory.path().join("shared")).expect("the shared path is UTF-8");
        let base = shared.join("cache");

        create_private_dir_all(&base).expect("the cache directory");
        fs::set_permissions(shared.as_std_path(), fs::Permissions::from_mode(0o1777)).expect("make the parent sticky and shared");

        let claimed = claim_redirected_cache(&source, &base);

        fs::set_permissions(shared.as_std_path(), fs::Permissions::from_mode(0o755)).expect("restore the parent");

        let _lock = claimed.expect("a sticky shared ancestor is not a foreign writer");
    }

    #[cfg(unix)]
    #[test]
    fn a_sticky_world_writable_cache_itself_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = private_system_tempdir("sticky-base-cache-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the source path is UTF-8");
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).expect("the cache path is UTF-8");

        fs::create_dir_all(&base).expect("the cache directory");
        fs::set_permissions(base.as_std_path(), fs::Permissions::from_mode(0o1777)).expect("make the cache sticky and shared");

        let failure = claim_redirected_cache(&source, &base).expect_err("a shared cache must be refused even when sticky");

        assert!(failure.is_usage());
        assert!(failure.to_string().contains("is writable by other users"), "{failure}");
        assert!(!base.join(CACHE_OWNER).exists());
        assert!(!base.join("lock").exists());

        fs::set_permissions(base.as_std_path(), fs::Permissions::from_mode(0o755)).expect("restore the cache");
    }

    /// The owner marker is a path a would-be attacker controls the contents of, and it is read
    /// back into a message printed to a terminal.
    #[test]
    fn a_hostile_owner_marker_cannot_address_the_terminal_it_is_reported_on() {
        let directory = private_system_tempdir("hostile-marker-cache-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the source path is UTF-8");
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).expect("the cache path is UTF-8");
        let mut planted = "/w\r\u{1b}[2Kforged".to_owned();
        let expected_len = physical(&source).as_str().len();
        planted.extend(core::iter::repeat_n('x', expected_len.saturating_sub(planted.len())));

        create_private_dir_all(&base).expect("the cache directory");
        fs::write(base.join(CACHE_OWNER).as_std_path(), planted).expect("the planted marker");

        let failure = claim_redirected_cache(&source, &base).expect_err("a foreign marker must be refused");
        let text = failure.to_string();

        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\r'), "{text:?}");
        assert!(text.contains("\\r\\e[2Kforged"), "{text:?}");
    }

    /// An impossible owner-marker length is rejected without reading or echoing its contents.
    #[test]
    fn an_oversized_owner_marker_is_rejected_before_its_contents_are_reported() {
        let directory = private_system_tempdir("oversized-marker-cache-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the source path is UTF-8");
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).expect("the cache path is UTF-8");

        create_private_dir_all(&base).expect("the cache directory");
        let oversized = usize::try_from(MAX_CACHE_OWNER_LEN)
            .expect("the marker bound fits usize on every supported target")
            .saturating_add(1);
        fs::write(base.join(CACHE_OWNER).as_std_path(), "x".repeat(oversized)).expect("the planted marker");

        let failure = claim_redirected_cache(&source, &base).expect_err("an oversized marker must be refused");
        let text = failure.to_string();

        assert!(text.contains("invalid length"), "{text:?}");
        assert!(!text.contains("xxxxx"), "{text:?}");
    }

    #[test]
    fn two_workspaces_cannot_use_one_redirected_cache_concurrently() {
        let directory = private_system_tempdir("contended-cache-");
        let first = Utf8PathBuf::from_path_buf(directory.path().join("first")).expect("the source path is UTF-8");
        let second = Utf8PathBuf::from_path_buf(directory.path().join("second")).expect("the source path is UTF-8");
        let base = Utf8PathBuf::from_path_buf(directory.path().join("cache")).expect("the cache path is UTF-8");

        let _held = claim_redirected_cache(&first, &base).expect("the first workspace claims the cache");
        let failure = claim_redirected_cache(&second, &base).expect_err("the live cache lock must serialize workspaces");

        assert!(failure.is_usage());
        assert!(failure.to_string().contains("already using"), "{failure}");
    }

    #[test]
    fn explicitly_naming_the_default_cache_does_not_take_its_lock_twice() {
        let directory = crate::testing::workdir("default-cache-alias-");
        let source = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the source path is UTF-8");
        let base = gamma_base(&source, None);
        let target = source.join("target");

        let CacheLocks {
            workspace: _workspace,
            redirected,
            #[cfg(any(test, feature = "internals"))]
                identity: _identity,
        } = claim_cache(&source, &target, Some(&base)).expect("the default cache has one lock domain");

        assert!(redirected.is_none());
    }

    #[test]
    fn cleaning_removes_cache_data_but_not_published_reports() {
        let directory = crate::testing::workdir("clean-cache-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the workspace path is UTF-8");
        let target = root.join("target");
        let scratch = gamma_base(&root, None);
        let base = campaign_base(&root, &target, None);
        let report = root.join("target/cargo-gamma/gamma-report.json");

        fs::create_dir_all(&scratch).expect("scratch");
        validate_cache_owner(&root, &scratch, CacheKind::Default).expect("claim scratch");
        fs::create_dir(scratch.join("workspace")).expect("cached workspace");
        fs::create_dir_all(&base).expect("cache");
        validate_cache_owner(&root, &base, CacheKind::Default).expect("claim cache");
        fs::create_dir_all(base.join("target")).expect("cached target");
        fs::write(base.join("last-gamma-run.json"), "{}").expect("run record");
        fs::create_dir_all(report.parent().expect("report directory")).expect("report directory");
        fs::write(&report, "{}").expect("published report");

        assert!(clean_cache(&root, &target).expect("clean cache"));
        assert!(!scratch.join("workspace").exists());
        assert!(!base.join("target").exists());
        assert!(!base.join("last-gamma-run.json").exists());
        assert!(scratch.join("lock").exists(), "the concurrency lock remains");
        assert!(report.exists(), "published output is not cache data");
        assert!(!clean_cache(&root, &target).expect("cleaning an empty cache"));
    }

    #[test]
    fn cleaning_refuses_an_unmarked_populated_cache() {
        let directory = crate::testing::workdir("clean-unmarked-cache-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the workspace path is UTF-8");
        let target = root.join("target");
        let base = campaign_base(&root, &target, None);

        fs::create_dir_all(base.join("target")).expect("unowned contents");

        let failure = clean_cache(&root, &target).expect_err("unowned contents must not be removed");

        assert!(failure.is_usage(), "{failure}");
        assert!(base.join("target").exists(), "unowned contents must survive");
        assert!(!base.join(CACHE_OWNER).exists(), "the cache must not be adopted");
    }

    #[test]
    fn cleaning_refuses_a_cache_owned_by_an_active_run() {
        let directory = crate::testing::workdir("clean-active-cache-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the workspace path is UTF-8");
        let target = root.join("target");
        let base = gamma_base(&root, None);

        fs::create_dir_all(&base).expect("cache");
        let _held = claim(&base).expect("active run lock");
        let failure = clean_cache(&root, &target).expect_err("an active cache must not be cleaned");

        assert!(failure.to_string().contains("already using"), "{failure}");
    }

    #[test]
    fn the_vendored_runtime_is_the_real_one() {
        // If these ever diverge, guards would be compiled against a runtime that is not the one
        // this build was tested with.
        let runtime = gamma_rt::embedded::SOURCES
            .iter()
            .find_map(|(name, source)| (*name == "runtime.rs").then_some(*source))
            .expect("runtime.rs is one of the embedded runtime sources");

        assert!(runtime.contains("pub fn a(id: u32) -> bool"));
        assert!(runtime.contains("GAMMA_ACTIVE"));
    }

    #[test]
    fn vendoring_writes_a_buildable_crate() {
        let temporary = tempfile::tempdir().unwrap();
        let at = Utf8PathBuf::from_path_buf(temporary.path().join("rt")).unwrap();

        vendor_runtime(&at).unwrap();

        let manifest = fs::read_to_string(at.join("Cargo.toml").as_std_path()).unwrap();

        assert!(manifest.contains("name = \"cargo-gamma-rt\""));
        assert!(manifest.contains("[lib]\nname = \"gamma_rt\""));
        assert!(manifest.contains("edition = \"2024\""));
        assert!(manifest.contains("rust-version = \"1.95\""));
        assert!(manifest.contains("check-cfg = ['cfg(coverage_nightly)', 'cfg(loom)']"));

        // The `[workspace]` table keeps it from being adopted by whatever workspace it lands near.
        assert!(manifest.contains("[workspace]"));
        for (name, _contents) in gamma_rt::embedded::SOURCES {
            assert!(at.join("src").join(name).as_std_path().is_file(), "{name} was not vendored");
        }

        let checked = Command::new(cargo_binary())
            .args(["check", "--offline", "--manifest-path"])
            .arg(at.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", at.join("target"))
            .output()
            .expect("cargo checks the vendored runtime");

        assert!(checked.status.success(), "{}", String::from_utf8_lossy(&checked.stderr));
    }

    /// A vendor location that cannot be created at all — because something is already sitting where
    /// its `src` directory would go — must fail with a message naming the directory rather than a
    /// bare `NotADirectory` the reader has to reverse-engineer back to what this function was doing.
    #[test]
    fn vendoring_into_a_location_whose_src_directory_cannot_be_created_reports_the_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let at = Utf8PathBuf::from_path_buf(temporary.path().join("rt")).unwrap();

        // A plain file standing where `at/src` needs to go blocks `create_dir_all` outright.
        fs::create_dir_all(at.as_std_path()).unwrap();
        fs::write(at.join("src").as_std_path(), "not a directory").unwrap();

        let cause = vendor_runtime(&at).unwrap_err();

        assert!(cause.to_string().contains("could not create"), "{cause}");
    }

    /// A vendor location whose manifest path is blocked by an existing directory cannot have the
    /// generated manifest written to it, and that has to be reported as the write failure it is.
    #[test]
    fn vendoring_a_manifest_blocked_by_a_directory_reports_the_write_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let at = Utf8PathBuf::from_path_buf(temporary.path().join("rt")).unwrap();

        fs::create_dir_all(at.join("Cargo.toml").as_std_path()).unwrap();

        let cause = vendor_runtime(&at).unwrap_err();

        assert!(cause.to_string().contains("could not write the runtime manifest"), "{cause}");
    }

    /// A vendor location whose runtime source path is blocked by an existing directory cannot have
    /// the vendored source written to it, and that has to be reported as the write failure it is.
    #[test]
    fn vendoring_a_source_file_blocked_by_a_directory_reports_the_write_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let at = Utf8PathBuf::from_path_buf(temporary.path().join("rt")).unwrap();

        fs::create_dir_all(at.join("src").join("lib.rs").as_std_path()).unwrap();

        let cause = vendor_runtime(&at).unwrap_err();

        assert!(cause.to_string().contains("could not write the runtime source"), "{cause}");
    }

    #[test]
    fn a_run_that_never_settled_takes_its_build_output_with_it() {
        // Artifacts of a tree that no longer exists cannot make anything incremental, and on a
        // large workspace they are tens of gigabytes on a disk a CI job still has plans for.
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();
        fs::write(target.join("artifact").as_std_path(), "x").unwrap();

        drop(unsettled(&base, &root, &target));

        assert!(!root.as_std_path().exists());
        assert!(!target.as_std_path().exists());
    }

    #[test]
    fn a_run_that_settled_keeps_its_tree_and_build_output_for_the_next_one() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();

        let work = unsettled(&base, &root, &target);

        work.settle();
        drop(work);

        assert!(root.as_std_path().exists(), "the tree is kept for delta sync on the next run");
        assert!(target.as_std_path().exists());
    }

    #[test]
    fn a_leaked_tree_keeps_everything() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();

        let mut work = unsettled(&base, &root, &target);

        work.leak = true;
        drop(work);

        assert!(root.as_std_path().exists());
        assert!(target.as_std_path().exists());
    }

    /// An explicit teardown is what a caller drives instead of waiting on the destructor.
    ///
    /// It has to remove exactly what dropping would have removed, and leave the destructor with
    /// nothing to repeat — a second walk of a tree that is already gone would report a failure for
    /// work that succeeded, and on a real tree the first walk is the expensive one.
    #[test]
    fn an_explicit_teardown_removes_the_tree_and_leaves_the_destructor_nothing_to_do() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();

        let mut work = unsettled(&base, &root, &target);

        work.teardown().expect("a tree that exists must tear down cleanly");

        assert!(!root.as_std_path().exists());
        assert!(!target.as_std_path().exists());
        assert!(work.torn_down, "the destructor would walk the tree a second time");

        // Recreated behind the workspace's back: if dropping still removed things, this would go.
        fs::create_dir_all(root.as_std_path()).unwrap();
        drop(work);

        assert!(root.as_std_path().exists(), "the destructor repeated a teardown already done");
    }

    /// Tearing down twice is the same answer twice, because an error path may have done it already.
    #[test]
    fn a_second_teardown_is_not_a_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();

        let mut work = unsettled(&base, &root, &target);

        work.teardown().expect("the first teardown");
        work.teardown().expect("the second teardown must agree with the first");
    }

    /// A settled run's build output survives an explicit teardown for the same reason it survives
    /// the destructor: it is what makes the next run incremental.
    #[test]
    fn an_explicit_teardown_keeps_the_tree_and_build_output_of_a_settled_run() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();

        let mut work = unsettled(&base, &root, &target);

        work.settle();
        work.teardown().expect("a settled tree must tear down cleanly");

        assert!(root.as_std_path().exists(), "the tree is kept for delta sync");
        assert!(target.as_std_path().exists());
    }

    /// `--leak-dirs` exists so the tree is still there afterwards, which an explicit teardown must
    /// respect as much as the destructor does.
    #[test]
    fn an_explicit_teardown_keeps_a_leaked_tree() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();

        let mut work = unsettled(&base, &root, &target);

        work.leak = true;
        work.teardown().expect("a leaked tree tears down by leaving everything alone");

        assert!(root.as_std_path().exists());
        assert!(target.as_std_path().exists());
    }

    /// A tree that cannot be removed is reported rather than swallowed — the whole reason for
    /// having an explicit teardown beside the destructor.
    #[test]
    fn a_tree_that_cannot_be_removed_is_reported() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");

        // A file where a directory is expected: `remove_dir_all` refuses it, and it is the same
        // shape of leftover that `prepare` already has to refuse.
        fs::write(root.as_std_path(), "not a directory").unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();

        let mut work = unsettled(&base, &root, &target);
        let failure = work.teardown().expect_err("a tree that is a file cannot be removed");

        assert!(failure.to_string().contains("could not remove the scratch directory"), "{failure}");

        // Reported, not abandoned part-way: the build directory is still cleared.
        assert!(!target.as_std_path().exists(), "the failure stopped the rest of the teardown");
    }

    /// A tree that was never created is not a failure to report: a run can fail before the copy.
    #[test]
    fn a_teardown_of_a_tree_that_was_never_created_succeeds() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");

        let mut work = unsettled(&base, &root, &target);

        work.teardown().expect("nothing to remove is the outcome asked for");
    }

    #[test]
    fn the_footprint_counts_everything_the_run_leaves_behind() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
        let root = base.join("workspace");
        let target = base.join("target");

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(target.as_std_path()).unwrap();
        fs::write(root.join("a.rs").as_std_path(), "0123456789").unwrap();
        fs::write(target.join("a.o").as_std_path(), "01234").unwrap();

        let mut work = unsettled(&base, &root, &target);

        assert_eq!(footprint(work.base()), 15);

        work.leak = true;
    }

    /// A workspace over a real directory that has not been marked as worth keeping.
    fn unsettled(base: &Utf8Path, root: &Utf8Path, target: &Utf8Path) -> Workspace {
        Workspace {
            base: base.to_owned(),
            root: root.to_owned(),
            runtime: base.join("rt"),
            target: target.to_owned(),
            rustc_captures: None,
            rustc_captures_ready: AtomicBool::new(false),
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            nextest: None,
            test_environment: None,
            cargo_metadata: None,
            settled: AtomicBool::new(false),
            leak: false,
            launch: OnceLock::new(),
            harness_threads: OnceLock::new(),
            resources: Resources::default(),
            testing_journal: None,
            #[cfg(test)]
            live_sweep_publisher: None,
            test_cache_home: crate::testing::cache_home(root),
            _workspace_lock: Some(File::create(base.join("lock").as_std_path()).unwrap()),
            _cache_lock: None,
            torn_down: false,
        }
    }

    /// The harness width is settled once and then handed out, without touching this process.
    ///
    /// Publishing it with `set_var` on the run's own environment would be sound for the real binary
    /// — one run, one thread, no children yet — and unsound for this suite, which calls `run` from
    /// forty tests at once while every other thread in the process is in `getenv`.
    ///
    /// The ambient variable `calibrate_harness` reads is controlled by launching a child with it
    /// unset rather than by clearing it on this shared, multithreaded process, so the read races
    /// nothing and there is nothing to restore afterwards.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn the_harness_width_is_settled_on_the_workspace_rather_than_on_this_process() {
        if env::var_os(crate::exec::UNDER_GAMMA_VAR).is_some() {
            return;
        }

        let child = run_child("calibrate", &[(TEST_THREADS_VAR, None), (CHILD_JOBS_VAR, Some("1"))]);

        assert_eq!(child["before"], PAYLOAD_MISSING, "nothing is settled before calibration");
        assert_eq!(
            child["ambient"], PAYLOAD_MISSING,
            "calibrating must not write the variable into the process environment"
        );
        assert_eq!(child["threads"], child["cores"], "one worker gets the whole machine");
    }

    /// A width the caller chose is left alone, and so nothing is set on the launched command.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_caller_who_chose_a_harness_width_gets_no_setting_from_the_run() {
        if env::var_os(crate::exec::UNDER_GAMMA_VAR).is_some() {
            return;
        }

        let child = run_child("calibrate", &[(TEST_THREADS_VAR, Some("3")), (CHILD_JOBS_VAR, Some("4"))]);

        assert_eq!(child["threads"], PAYLOAD_MISSING, "the caller's choice stands");
    }

    #[test]
    fn the_build_cannot_see_an_active_mutant() {
        // A live mutant inside a proc macro would run inside rustc and could hang the one build
        // the whole run depends on.
        let work = unsettled_default();

        // `Workspace::cargo` reads `CARGO_ENCODED_RUSTFLAGS` and `RUSTFLAGS`, but only reads them,
        // and no test in this binary writes the process environment, so the read races nothing.
        let command = work.cargo();
        let scrubbed = command
            .get_envs()
            .any(|(key, value)| key == gamma_rt::ACTIVE_VAR && value.is_none());

        assert!(scrubbed, "the build environment must not carry {}", gamma_rt::ACTIVE_VAR);
    }

    /// Selects which scenario an [`env_child_helper`] subprocess should run.
    ///
    /// Unset in an ordinary suite run, so the helper returns at once; set by [`run_child`] on the
    /// re-executed test binary, whose inherited environment is then the only thing the code under
    /// test reads.
    const CHILD_SCENARIO_VAR: &str = "GAMMA_ENV_CHILD_SCENARIO";

    /// Carries the worker count into the `calibrate` scenario, since `calibrate_harness` takes it
    /// as an argument rather than reading it from the environment.
    const CHILD_JOBS_VAR: &str = "GAMMA_ENV_CHILD_JOBS";

    /// Opens the `key=value` block [`env_child_helper`] prints, so the parent can lift it out of
    /// the test harness's own output on the shared stdout.
    const PAYLOAD_OPEN: &str = "<<<GAMMA-ENV-PAYLOAD";

    /// Closes the block opened by [`PAYLOAD_OPEN`].
    const PAYLOAD_CLOSE: &str = "GAMMA-ENV-PAYLOAD>>>";

    /// Payload marker for a variable or value that was absent.
    const PAYLOAD_MISSING: &str = "<missing>";

    /// Payload marker for an environment entry a `Command` explicitly clears.
    const PAYLOAD_REMOVED: &str = "<removed>";

    /// The subprocess half of the environment tests.
    ///
    /// A `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` spelling, for a triple no host builds for.
    ///
    /// Deliberately not the triple in force: `cap_ambient_rustflags` extends every such variable
    /// rather than the one cargo will consult, because the triple is not settled where it runs, and
    /// a test naming the host's own triple could not tell the two behaviours apart.
    const TARGET_RUSTFLAGS_VAR: &str = "CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS";

    /// The variables the code under test reads — `CARGO`, `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`,
    /// `RUST_TEST_THREADS` — are process-global and read without a lock, so a test that set them by
    /// mutating this multithreaded binary would race every other thread already inside `getenv`.
    /// This process is instead re-executed by [`run_child`] with those variables set on its
    /// `Command`, so the values are the ones it was *launched* with and nothing ever writes the
    /// running environment. The child only ever reads the environment it inherited, so its own
    /// threads cannot race one another over it either.
    ///
    /// In an ordinary suite run the scenario variable is unset and this returns at once.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[expect(clippy::too_many_lines, reason = "isolated environment scenarios share one subprocess entry point")]
    fn env_child_helper() {
        let Ok(scenario) = env::var(CHILD_SCENARIO_VAR) else {
            return;
        };

        let mut payload: Vec<(&str, String)> = Vec::new();

        match scenario.as_str() {
            "cargo_binary" => payload.push(("cargo", cargo_binary())),
            "wrapper_capture" => {
                let directory = private_system_tempdir("workspace-parent-capture-child-");
                let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
                crate::testing::write_project(
                    root.as_std_path(),
                    &[
                        ("Cargo.toml", "[package]\nname='capture'\nversion='0.0.0'\n[workspace]\n"),
                        ("src/lib.rs", ""),
                    ],
                );
                let mut work = Workspace::prepare(
                    &root,
                    &root.join("target"),
                    &Config::default(),
                    &mut crate::testing::Recorder::default(),
                )
                .unwrap();
                let captures = work.target.join(".cargo-gamma-rustc");
                assert_eq!(work.rustc_captures.as_ref(), Some(&captures));
                assert!(work.rustc_captures().is_none());
                fs::create_dir_all(&captures).unwrap();
                fs::write(captures.join("stale.json"), "previous generation").unwrap();
                work.begin_build_sequence();
                assert_eq!(work.rustc_captures(), Some(captures.clone()));
                assert!(!captures.join("stale.json").exists());
                let private_home = root.join("explicit-private-home");
                work.test_cache_home = Some(private_home.clone());
                let command = work.cargo();
                let wrapper = wrapper_path().expect("the copied test executable has cargo-gamma's exact filename");
                assert_eq!(command_env(&command, "RUSTC_WRAPPER"), wrapper.as_str());
                assert_eq!(command_env(&command, CAPTURE_DIR_VAR), captures.as_str());
                assert_eq!(command_env(&command, TEST_CACHE_HOME_VAR), private_home.as_str());
                let expected_outer = env::var("RUSTC_WRAPPER")
                    .ok()
                    .filter(|outer| !wrapper_is_self(OsStr::new(outer), &wrapper, &work.root));
                assert_eq!(
                    command_env(&command, ORIGINAL_WRAPPER_VAR),
                    expected_outer.as_deref().unwrap_or(PAYLOAD_REMOVED)
                );
                payload.push(("capture", "retained".to_owned()));
                work.teardown().unwrap();
            }
            "cache_claims" => crate::testing::with_ambient_cache_home(|| {
                let directory = private_system_tempdir("workspace-parent-claim-child-");
                let root = absolute(Utf8Path::from_path(directory.path()).unwrap());
                let target = root.join("artifacts");
                let scratch = gamma_base(&root, None);
                let campaign = campaign_base(&root, &target, None);
                let redirected = root.join("redirected");
                let mut identities = Vec::new();
                for cache in [None, Some(&scratch), Some(&campaign), Some(&redirected)] {
                    let locks = claim_cache(&root, &target, cache.map(Utf8PathBuf::as_path)).unwrap();
                    assert_eq!(locks.redirected.is_some(), cache == Some(&redirected));
                    assert!(
                        !identities.contains(&locks.identity),
                        "every claimed handoff has a distinct identity"
                    );
                    identities.push(locks.identity);
                }
                for cache in [None, Some(redirected.as_path())] {
                    let base = campaign_base(&root, &target, cache);
                    fs::write(base.join(CACHE_OWNER), "a foreign workspace").unwrap();
                    let failure = claim_cache(&root, &target, cache)
                        .err()
                        .expect("a foreign campaign owner must be returned as an error, not unwrapped");
                    assert!(failure.message().contains("belongs to the workspace"), "{failure}");
                    fs::write(base.join(CACHE_OWNER), physical(&root).as_str()).unwrap();
                }
                payload.push(("claims", identities.len().to_string()));
            }),
            "cache_paths" | "campaign_creation_refusal" => crate::testing::with_ambient_cache_home(|| {
                let directory = private_system_tempdir("workspace-parent-child-cache-");
                let root = Utf8Path::from_path(directory.path()).unwrap();
                let target = root.join("artifacts");
                let scratch = gamma_base(root, None);
                let campaign = absolute(&default_campaign_base(root, &target));
                payload.push(("root", root.to_string()));
                payload.push(("target", target.to_string()));
                payload.push(("identity", workspace_identity(root)));
                payload.push(("home", default_cache_home(root).to_string()));
                payload.push(("scratch", scratch.to_string()));
                payload.push(("campaign", campaign.to_string()));
                validate_default_cache_base(root, &scratch).unwrap();
                validate_default_campaign_base(root, &target, &campaign).unwrap();
                assert!(validate_default_campaign_base(root, &target, &root.join("foreign")).is_err());
                if scenario == "campaign_creation_refusal" {
                    crate::testing::write_project(
                        root.as_std_path(),
                        &[
                            ("Cargo.toml", "[package]\nname='child'\nversion='0.0.0'\n[workspace]\n"),
                            ("src/lib.rs", ""),
                        ],
                    );
                    fs::write(&target, "not a target directory").unwrap();
                    let failure = Workspace::prepare(root, &target, &Config::default(), &mut crate::testing::Recorder::default())
                        .expect_err("the campaign directory cannot be created beneath a file");
                    payload.push(("error", failure.message().to_owned()));
                    assert_eq!(failure.message(), format!("could not create the campaign cache at `{campaign}`"));
                    assert_eq!(fs::read_to_string(&target).unwrap(), "not a target directory");
                    let failure = claim_default_campaign_cache(root, &target, &campaign).unwrap_err();
                    assert_eq!(
                        failure.message(),
                        format!("could not create cargo-gamma's campaign cache at `{campaign}`")
                    );
                }
            }),
            "calibrate_exact" => {
                let cores = NonZeroUsize::new(8).unwrap();
                let work = unsettled_default();
                assert!(work.harness_threads().is_none());
                work.calibrate_harness_with(cores.get(), Ok(cores));
                assert_eq!(work.harness_threads(), Some("1"));
                payload.push(("cores", cores.to_string()));
                payload.push(("threads", option_payload(work.harness_threads())));
                work.calibrate_harness_with(1, Ok(cores));
                assert_eq!(work.harness_threads(), Some("1"));
            }
            "calibrate_unavailable" => {
                let directory = private_system_tempdir("workspace-parent-unavailable-cores-");
                let unavailable = File::open(directory.path().join("absent")).map(|_| NonZeroUsize::MIN);
                assert!(unavailable.is_err());
                let work = unsettled_default();
                work.calibrate_harness_with(1, unavailable);
                payload.push(("threads", option_payload(work.harness_threads())));
            }
            "cargo_flags" => {
                let command = unsettled_default().cargo();

                payload.push(("encoded", command_env(&command, "CARGO_ENCODED_RUSTFLAGS")));
                payload.push(("rustflags", command_env(&command, "RUSTFLAGS")));
                payload.push(("build", command_env(&command, "CARGO_BUILD_RUSTFLAGS")));
                payload.push(("target", command_env(&command, TARGET_RUSTFLAGS_VAR)));
            }
            "calibrate" => {
                let jobs = env::var(CHILD_JOBS_VAR).ok().and_then(|value| value.parse().ok()).unwrap_or(1);
                let work = unsettled_default();

                payload.push(("before", option_payload(work.harness_threads())));
                work.calibrate_harness(jobs);
                payload.push(("threads", option_payload(work.harness_threads())));
                payload.push(("cores", thread::available_parallelism().map_or(1, NonZeroUsize::get).to_string()));
                payload.push((
                    "ambient",
                    env::var(TEST_THREADS_VAR).unwrap_or_else(|_absent| PAYLOAD_MISSING.to_owned()),
                ));
            }
            other => panic!("unknown child scenario `{other}`"),
        }

        let mut printed = format!("{PAYLOAD_OPEN}\n");

        for (key, value) in &payload {
            printed.push_str(key);
            printed.push('=');
            printed.push_str(value);
            printed.push('\n');
        }

        printed.push_str(PAYLOAD_CLOSE);
        println!("{printed}");
    }

    /// Re-executes this test binary filtered to [`env_child_helper`] with `scenario` selected and
    /// `vars` applied to the child's environment, returning the `key=value` payload it printed.
    ///
    /// The point is isolation: the variables are set on the child's `Command`, so the code under
    /// test reads exactly the values this process chose without any thread ever writing the running
    /// environment — the data race that made a safe `set_var` wrapper unsound in the first place.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn run_child(scenario: &str, vars: &[(&str, Option<&str>)]) -> BTreeMap<String, String> {
        let executable = env::current_exe().expect("the test binary knows its own path");
        run_child_at(&executable, scenario, vars)
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn run_child_at(executable: &Path, scenario: &str, vars: &[(&str, Option<&str>)]) -> BTreeMap<String, String> {
        let mut command = Command::new(executable);

        // libtest names a test by its module path with the crate segment stripped, e.g.
        // `exec::workspace::tests::env_child_helper`; `--exact` needs that whole name, not the bare
        // function. `module_path!` gives the crate-qualified path, so drop the leading crate.
        let module = module_path!();
        let relative = module.split_once("::").map_or(module, |(_crate_name, rest)| rest);
        let target = format!("{relative}::env_child_helper");

        let _ = command.args([target.as_str(), "--exact", "--nocapture"]);
        let _ = command.env(CHILD_SCENARIO_VAR, scenario);

        for (key, value) in vars {
            match value {
                Some(value) => {
                    let _ = command.env(key, value);
                }
                None => {
                    let _ = command.env_remove(key);
                }
            }
        }

        let output = command.output().expect("the child test binary runs");
        let stdout = String::from_utf8(output.stdout).expect("the child prints UTF-8");
        if let Some(payload) = parse_payload(&stdout) {
            return payload;
        }

        panic!(
            "the `env_child_helper` child produced no payload (status {status}); stdout:\n{stdout}\nstderr:\n{stderr}",
            status = output.status,
            stderr = String::from_utf8_lossy(&output.stderr),
        );
    }

    /// Lifts the `key=value` block [`env_child_helper`] prints out of the harness's surrounding
    /// output, or `None` if the child never emitted a complete block.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn parse_payload(stdout: &str) -> Option<BTreeMap<String, String>> {
        // Single-threaded libtest can print the test name before the opening marker.
        let mut lines = stdout.lines().skip_while(|line| !line.ends_with(PAYLOAD_OPEN));

        // Consume the PAYLOAD_OPEN marker itself; bail if the child never emitted one.
        let _ = lines.next()?;

        let mut payload = BTreeMap::new();

        for line in lines {
            if line == PAYLOAD_CLOSE {
                return Some(payload);
            }

            if let Some((key, value)) = line.split_once('=') {
                let _ = payload.insert(key.to_owned(), value.to_owned());
            }
        }

        None
    }

    #[test]
    fn child_payload_parser_accepts_standalone_and_libtest_prefixed_markers() {
        let expected = BTreeMap::from([
            ("cargo".to_owned(), "distinct-invoking-cargo".to_owned()),
            ("flags".to_owned(), "--cfg=value".to_owned()),
            ("empty".to_owned(), String::new()),
        ]);
        for stdout in [
            "\nrunning 1 test\n<<<GAMMA-ENV-PAYLOAD\ncargo=distinct-invoking-cargo\nflags=--cfg=value\nempty=\nGAMMA-ENV-PAYLOAD>>>\nok\n",
            "\r\nrunning 1 test\r\ntest exec::workspace::tests::env_child_helper ... <<<GAMMA-ENV-PAYLOAD\r\ncargo=distinct-invoking-cargo\r\nflags=--cfg=value\r\nempty=\r\nGAMMA-ENV-PAYLOAD>>>\r\nok\r\n",
        ] {
            assert_eq!(parse_payload(stdout), Some(expected.clone()), "{stdout:?}");
        }
    }

    #[test]
    fn child_payload_parser_rejects_missing_or_incomplete_markers() {
        for stdout in [
            "",
            "running 0 tests\n",
            "cargo=distinct-invoking-cargo\nGAMMA-ENV-PAYLOAD>>>\n",
            "<<<GAMMA-ENV-PAYLOAD\ncargo=distinct-invoking-cargo\n",
            "<<<GAMMA-ENV-PAYLOAD-extra\ncargo=distinct-invoking-cargo\nGAMMA-ENV-PAYLOAD>>>\n",
            "<<<GAMMA-ENV-PAYLOAD\ncargo=distinct-invoking-cargo\nGAMMA-ENV-PAYLOAD>>>-extra\n",
        ] {
            assert_eq!(parse_payload(stdout), None, "{stdout:?}");
        }
    }

    /// The value a built `Command` carries for `key`: the string it sets, [`PAYLOAD_REMOVED`] if it
    /// explicitly clears it, or [`PAYLOAD_MISSING`] if it neither sets nor mentions it. Lets a
    /// subprocess report a `Command`'s environment back to the parent as text.
    fn command_env(command: &Command, key: &str) -> String {
        command.get_envs().find(|(name, _value)| *name == OsStr::new(key)).map_or_else(
            || PAYLOAD_MISSING.to_owned(),
            |(_name, value)| value.map_or_else(|| PAYLOAD_REMOVED.to_owned(), |value| value.to_string_lossy().into_owned()),
        )
    }

    /// Renders an optional harness width for the payload: the value, or [`PAYLOAD_MISSING`] for
    /// `None`.
    fn option_payload(value: Option<&str>) -> String {
        value.map_or_else(|| PAYLOAD_MISSING.to_owned(), ToOwned::to_owned)
    }

    /// The tests re-execute a child instead of mutating this multithreaded parent precisely so that
    /// a value one test needs cannot contaminate — or race a reader in — another. This proves both
    /// halves: a distinctive value is injected into the child, the child reads it back through the
    /// code under test, and the parent's own environment is shown to be exactly what it was before.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn the_child_helper_isolates_the_environment_from_the_parent() {
        // A value the parent does not carry: a process-wide write of it would be visible here both
        // before and after, and racing to spawn a real cargo elsewhere with it would break.
        const INJECTED: &str = "/gamma/isolated/child/only/cargo";

        if env::var_os(crate::exec::UNDER_GAMMA_VAR).is_some() {
            return;
        }

        let before = env::var_os("CARGO");
        let child = run_child("cargo_binary", &[("CARGO", Some(INJECTED))]);

        assert_eq!(child["cargo"], INJECTED, "the child read the value it was launched with");
        assert_eq!(env::var_os("CARGO"), before, "the child's environment did not leak into the parent");
    }

    /// Honouring `CARGO` matters because the toolchain that invoked this run may not be the one on
    /// `PATH` — `rustup` overrides, pinned toolchains, and CI runners that vendor a specific cargo
    /// all set it — so falling back to the literal name `cargo` unconditionally would silently
    /// build with the wrong toolchain. Both the honoured and the fallback case have to work.
    ///
    /// A made-up path is safe as the honoured value: it is set on the child's `Command`, not on
    /// this process, so it cannot reach the other tests spawning real cargo processes beside this
    /// one.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn cargo_binary_prefers_the_invoking_cargo_but_falls_back_when_it_is_unset() {
        if env::var_os(crate::exec::UNDER_GAMMA_VAR).is_some() {
            return;
        }

        let honoured = run_child("cargo_binary", &[("CARGO", Some("/opt/pinned/toolchain/bin/cargo"))]);
        assert_eq!(
            honoured["cargo"], "/opt/pinned/toolchain/bin/cargo",
            "a set CARGO is honoured verbatim"
        );

        let fallback = run_child("cargo_binary", &[("CARGO", None)]);
        assert_eq!(fallback["cargo"], "cargo", "an unset CARGO falls back to the bare name");
    }

    #[test]
    fn isolated_cargo_callers_keep_the_exact_executable_flags_and_settled_harness_width_during_mutation() {
        let injected = run_child("cargo_binary", &[("CARGO", Some("distinct-invoking-cargo"))]);
        assert_eq!(injected["cargo"], "distinct-invoking-cargo");
        let missing = run_child("cargo_binary", &[("CARGO", None)]);
        assert_eq!(missing["cargo"], "cargo");
        let flags = run_child(
            "cargo_flags",
            &[("CARGO_ENCODED_RUSTFLAGS", Some("--cfg\u{1f}distinct")), ("RUSTFLAGS", None)],
        );
        assert_eq!(flags["encoded"], format!("--cfg\u{1f}distinct\u{1f}{CAP_LINTS}"));
        let calibration = run_child("calibrate_exact", &[(TEST_THREADS_VAR, None)]);
        assert_eq!(calibration["threads"], "1", "{calibration:?}");
        let unavailable = run_child("calibrate_unavailable", &[(TEST_THREADS_VAR, None)]);
        assert_eq!(unavailable["threads"], "1");
    }

    #[test]
    fn isolated_default_campaign_paths_keep_target_identity_and_cache_override_boundaries() {
        let directory = private_system_tempdir("workspace-parent-cache-environment-");
        let home = directory.path().to_str().unwrap();
        let no_override = [(TEST_CACHE_HOME_VAR, None), ("XDG_CACHE_HOME", Some(home))];
        let ordinary = run_child("cache_paths", &no_override);
        let target = Utf8Path::new(&ordinary["target"]);
        assert_eq!(
            Utf8Path::new(&ordinary["campaign"]),
            target.join("cargo-gamma").join("cache").join(&ordinary["identity"])
        );
        assert_eq!(Utf8Path::new(&ordinary["home"]), Utf8Path::new(home).join("cargo-gamma"));
        assert_eq!(
            Utf8Path::new(&ordinary["scratch"]),
            Utf8Path::new(home).join("cargo-gamma").join(&ordinary["identity"])
        );
        let overridden = run_child(
            "cache_paths",
            &[(TEST_CACHE_HOME_VAR, Some(home)), ("XDG_CACHE_HOME", Some("not-absolute"))],
        );
        assert_eq!(overridden["campaign"], overridden["scratch"]);
        assert_eq!(Utf8Path::new(&overridden["home"]), Utf8Path::new(home).join("cargo-gamma"));
        let refusal = run_child("campaign_creation_refusal", &no_override);
        assert_eq!(
            refusal["error"],
            format!("could not create the campaign cache at `{}`", refusal["campaign"])
        );
        let claims = run_child("cache_claims", &no_override);
        assert_eq!(claims["claims"], "4");
    }

    #[test]
    fn the_real_gamma_executable_derives_captures_and_interposes_each_outer_wrapper_shape() {
        let directory = private_system_tempdir("workspace-parent-copied-executable-");
        let executable = directory.path().join(format!("cargo-gamma{}", env::consts::EXE_SUFFIX));
        fs::copy(env::current_exe().unwrap(), &executable).unwrap();
        let self_wrapper = executable.to_str().unwrap();
        let cargo_home = Utf8Path::from_path(directory.path()).unwrap().join("cargo-home");
        fs::create_dir_all(&cargo_home).unwrap();
        for outer in [None, Some("distinct-outer-wrapper"), Some(self_wrapper)] {
            let child = run_child_at(
                &executable,
                "wrapper_capture",
                &[
                    ("RUSTC_WRAPPER", outer),
                    ("CARGO_BUILD_RUSTC_WRAPPER", None),
                    ("CARGO_HOME", Some(cargo_home.as_str())),
                ],
            );
            assert_eq!(child["capture"], "retained");
        }
    }

    /// [`unsettled_default`] over a real directory, for the tests that touch the tree.
    ///
    /// Spelled out rather than deriving from [`unsettled_default`] with struct update syntax:
    /// `Workspace` implements `Drop`, so its fields cannot be moved out of another instance.
    fn unsettled_at(root: Utf8PathBuf, runtime: Utf8PathBuf) -> Workspace {
        let test_cache_home = crate::testing::cache_home(&root);

        Workspace {
            base: Utf8PathBuf::from("/scratch"),
            root,
            runtime,
            target: Utf8PathBuf::from("/scratch/build"),
            rustc_captures: None,
            rustc_captures_ready: AtomicBool::new(false),
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            nextest: None,
            test_environment: None,
            cargo_metadata: None,
            settled: AtomicBool::new(true),
            leak: true,
            launch: OnceLock::new(),
            harness_threads: OnceLock::new(),
            resources: Resources::default(),
            testing_journal: None,
            #[cfg(test)]
            live_sweep_publisher: None,
            test_cache_home,
            _workspace_lock: Some(tempfile::tempfile().unwrap()),
            _cache_lock: None,
            torn_down: false,
        }
    }

    #[test]
    fn search_path_preparation_preserves_order_and_deduplicates_both_sources() {
        let mut work = unsettled_default();
        let root = work.root.clone();
        work.runtime = root.join("runtime");
        work.libraries = vec!["toolchain/deps".into(), "toolchain/deps".into(), "other".into()];
        let binaries = ["profile/deps/one", "profile/deps/two", "elsewhere/three"].map(crate::testing::test_binary);
        work.prepend_test_search_paths(&binaries);
        assert_eq!(
            work.libraries,
            ["profile", "profile/deps", "elsewhere", "toolchain", "toolchain/deps", "other"].map(Utf8PathBuf::from)
        );
        let once = work.libraries.clone();
        work.prepend_test_search_paths(&binaries);
        assert_eq!(work.libraries, once);
        assert_eq!(work.testing_roots().unwrap(), [root.clone(), root.join("runtime")]);
        work.runtime = root.clone();
        assert_eq!(work.testing_roots().unwrap(), [root]);
        assert!(work.arm_nextest(&[]).unwrap_err().message().contains("was not prepared"));
    }

    #[test]
    fn owner_lookup_rejects_nonregular_invalid_and_missing_markers_without_panicking() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8PathBuf::from_path_buf(fs::canonicalize(directory.path()).unwrap()).unwrap();
        let base = root.join("cache");
        assert_eq!(cache_owner_root(&base), None);
        fs::create_dir(&base).unwrap();
        assert_eq!(cache_owner_root(&base), None);
        let marker = base.join(CACHE_OWNER);
        fs::create_dir(&marker).unwrap();
        assert_eq!(cache_owner_root(&base), None);
        fs::remove_dir(&marker).unwrap();
        for contents in [b"relative".as_slice(), b"\xff", b""] {
            fs::write(&marker, contents).unwrap();
            assert_eq!(cache_owner_root(&base), None);
        }
        fs::write(&marker, root.as_str()).unwrap();
        assert_eq!(cache_owner_root(&base), Some(root));
        File::create(&marker).unwrap().set_len(MAX_CACHE_OWNER_LEN + 1).unwrap();
        assert_eq!(cache_owner_root(&base), None);
        fs::remove_file(&marker).unwrap();
        fs::remove_dir(&base).unwrap();
        fs::write(&base, "not a directory").unwrap();
        assert_eq!(cache_owner_root(&base), None);
    }

    #[test]
    fn owner_snapshot_gates_reject_replaced_nondirectories_and_nonregular_markers_before_reading_later_names() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = physical(Utf8Path::from_path(directory.path()).unwrap());
        for replace_base in [false, true] {
            let base = root.join(if replace_base { "replaced-base" } else { "replaced-marker" });
            let marker = base.join(CACHE_OWNER);
            if replace_base {
                fs::write(&base, "a file at inspection time").unwrap();
            } else {
                fs::create_dir_all(&marker).unwrap();
            }
            let mut calls = 0;
            let owner = cache_owner_root_with_metadata(&base, |path| {
                calls += 1;
                let metadata = fs::symlink_metadata(path)?;
                if replace_base && path == base {
                    fs::remove_file(&base)?;
                    fs::create_dir(&base)?;
                    fs::write(&marker, root.as_str())?;
                } else if !replace_base && path == marker {
                    fs::remove_dir(&marker)?;
                    fs::write(&marker, root.as_str())?;
                }
                Ok(metadata)
            });
            assert_eq!(owner, None, "the inspected identity must be checked before trusting a replacement");
            assert_eq!(calls, if replace_base { 1 } else { 2 });
            assert_eq!(fs::read_to_string(&marker).unwrap(), root.as_str());
        }
    }

    #[test]
    fn owner_marker_size_limit_accepts_the_last_two_lengths_and_rejects_the_first_excess_byte() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = physical(Utf8Path::from_path(directory.path()).unwrap());
        let base = root.join("cache");
        fs::create_dir(&base).unwrap();
        let marker = base.join(CACHE_OWNER);
        for length in [MAX_CACHE_OWNER_LEN - 1, MAX_CACHE_OWNER_LEN, MAX_CACHE_OWNER_LEN + 1] {
            let prefix = format!("{root}{}", std::path::MAIN_SEPARATOR);
            let remaining = usize::try_from(length).unwrap() - prefix.len();
            let recorded = Utf8PathBuf::from(format!("{prefix}{}", "x".repeat(remaining)));
            assert_eq!(recorded.as_str().len(), usize::try_from(length).unwrap());
            fs::write(&marker, recorded.as_str()).unwrap();
            if length <= MAX_CACHE_OWNER_LEN {
                assert_eq!(cache_owner_root(&base), Some(recorded.clone()));
                validate_cache_owner(&recorded, &base, CacheKind::Redirected).unwrap();
            } else {
                assert_eq!(cache_owner_root(&base), None);
                let failure = validate_cache_owner(&recorded, &base, CacheKind::Redirected)
                    .expect_err("the first byte beyond the bounded owner protocol is invalid");
                assert_eq!(
                    failure.message(),
                    format!(
                        "the cargo-gamma cache owner marker at `{marker}` has an invalid length.\nChoose a different directory for --cache-dir."
                    )
                );
            }
        }
    }

    #[test]
    fn owner_validation_rejects_postinspection_growth_and_truncation_and_reports_metadata_transport_failure() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = physical(Utf8Path::from_path(directory.path()).unwrap());
        let base = root.join("cache");
        fs::create_dir(&base).unwrap();
        let marker = base.join(CACHE_OWNER);
        let prefix = format!("{root}{}", std::path::MAIN_SEPARATOR);
        let recorded = Utf8PathBuf::from(format!(
            "{prefix}{}",
            "x".repeat(usize::try_from(MAX_CACHE_OWNER_LEN).unwrap() - prefix.len())
        ));
        for extra in [None, Some(b"x".as_slice()), Some(b"x\xff".as_slice())] {
            fs::write(&marker, recorded.as_str()).unwrap();
            let failure = validate_cache_owner_with_metadata(&recorded, &base, CacheKind::Redirected, |opened| {
                let metadata = opened.metadata()?;
                let mut writer = File::options().write(true).open(&marker)?;
                if let Some(extra) = extra {
                    let _ = std::io::Seek::seek(&mut writer, std::io::SeekFrom::End(0))?;
                    writer.write_all(extra)?;
                } else {
                    writer.set_len(MAX_CACHE_OWNER_LEN - 1)?;
                }
                Ok(metadata)
            })
            .expect_err("reading through the opened handle must reject a length different from its inspected snapshot");
            assert_eq!(
                failure.message(),
                format!(
                    "the cargo-gamma cache owner marker at `{marker}` has an invalid length.\nChoose a different directory for --cache-dir."
                )
            );
        }
        fs::write(&marker, root.as_str()).unwrap();
        let failure = validate_cache_owner_with_metadata(&root, &base, CacheKind::Default, |opened| {
            assert_eq!(opened.metadata()?.len(), u64::try_from(root.as_str().len()).unwrap());
            Err(io::Error::other("metadata transport refused after successful open"))
        })
        .expect_err("metadata failure after a successful marker open cannot become a successful claim");
        assert_eq!(
            failure.message(),
            format!("could not inspect the cargo-gamma cache owner marker at `{marker}`")
        );
    }

    #[test]
    #[cfg(unix)]
    fn opened_owner_directories_fail_the_native_handle_type_check_before_any_read() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = physical(Utf8Path::from_path(directory.path()).unwrap());
        let base = root.join("cache");
        let marker = base.join(CACHE_OWNER);
        fs::create_dir_all(&marker).unwrap();
        let failure = validate_cache_owner(&root, &base, CacheKind::Default)
            .expect_err("O_NONBLOCK allows a directory handle to open but cannot authorize it as a marker");
        assert_eq!(
            failure.message(),
            format!(
                "the cargo-gamma cache owner marker at `{marker}` is not a regular file.\nPass --cache-dir to give one of the two workspaces a cache of its own."
            )
        );
    }

    #[test]
    #[cfg(not(unix))]
    fn opening_an_owner_directory_retains_the_native_invalid_input_diagnostic() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let marker = root.join(CACHE_OWNER);
        fs::create_dir(&marker).unwrap();
        let failure = open_cache_owner(&marker).expect_err("the pre-open Windows type check must reject directories");
        assert_eq!(failure.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(failure.to_string(), "cache owner marker is not a regular file");
        fs::remove_dir(&marker).unwrap();
        fs::write(&marker, "regular marker").unwrap();
        let mut contents = String::new();
        open_cache_owner(&marker).unwrap().read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "regular marker");
    }

    #[test]
    fn testing_state_resolution_prefers_explicit_owners_then_searches_past_missing_child_locators() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = physical(Utf8Path::from_path(directory.path()).unwrap());
        crate::testing::with_cache_home_at(root.join("cache-home"), || {
            let child = root.join("member/src");
            fs::create_dir_all(&child).unwrap();
            let explicit = root.join("explicit");
            let located = root.join("located");
            for base in [&explicit, &located] {
                fs::create_dir(base).unwrap();
                mark_cache_owned_for_test(&root, base);
            }
            let locator = gamma_base(&root, None).join(CAMPAIGN_LOCATION);
            fs::create_dir_all(locator.parent().unwrap()).unwrap();
            mark_cache_owned_for_test(&root, locator.parent().unwrap());
            fs::write(&locator, format!("\n{located}\n")).unwrap();
            assert_eq!(
                locate_testing_state(&child, Some(&explicit)).unwrap(),
                (root.clone(), explicit.clone())
            );
            assert_eq!(locate_testing_state(&child, None).unwrap(), (root.clone(), located));
            let (claimed_root, claimed_base, lock) = claim_testing_state(&child, Some(&explicit)).unwrap();
            assert_eq!((claimed_root, claimed_base), (root.clone(), explicit.clone()));
            drop(lock);
            let failure = locate_testing_state(&child, Some(&root.join("missing")))
                .expect_err("an explicit ownerless cache cannot silently use the locator");
            assert_eq!(
                failure.message(),
                format!(
                    "cannot resume testing: `{}` has no valid campaign owner; start a new campaign",
                    root.join("missing")
                )
            );
            let outside = root.parent().unwrap();
            let failure = locate_testing_state(outside, Some(&explicit)).expect_err("a valid owner cannot authorize its parent directory");
            assert!(
                failure
                    .message()
                    .contains(&format!("belongs to `{root}`, not the selected directory")),
                "{failure}"
            );
        });
    }

    #[derive(Default)]
    struct PreparationEvents {
        calls: Vec<String>,
        refuse_log: bool,
    }

    impl Events for PreparationEvents {
        fn phase(&mut self, verb: &str, detail: &str) {
            self.calls.push(format!("phase:{verb}:{detail}"));
        }

        fn mutant(&mut self, _mutant: &crate::model::Mutant) {}

        fn begin(&mut self, active: &str, completed: &str, detail: &str) {
            self.calls.push(format!("begin:{active}:{completed}:{detail}"));
        }

        fn end(&mut self, detail: &str) {
            self.calls.push(format!("end:{detail}"));
        }

        fn testing_log(&mut self, base: &Utf8Path) -> Result<()> {
            self.calls.push(format!("log:{base}"));
            if self.refuse_log {
                Err(error!("testing log was refused"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn preparation_reports_exact_phase_boundaries_and_starts_with_unarmed_state_for_both_leak_policies() {
        for leak in [false, true] {
            let directory = crate::testing::workdir("exec-preparation-state-");
            let source = Utf8Path::from_path(directory.path()).unwrap();
            crate::testing::write_project(
                source.as_std_path(),
                &[
                    ("Cargo.toml", "[package]\nname='prepared'\nversion='0.0.0'\n[workspace]\n"),
                    ("src/lib.rs", "pub fn subject() {}\n"),
                ],
            );
            let config = Config {
                leak_dirs: leak,
                cargo: CargoOptions {
                    features: vec!["--features=nondefault".into()],
                    profile: Some("release".into()),
                    test_args: vec!["--exact".into(), "selected".into()],
                    ..CargoOptions::default()
                },
                ..Config::default()
            };
            let mut events = PreparationEvents::default();
            let mut work = Workspace::prepare(source, &source.join("target"), &config, &mut events).unwrap();
            assert_eq!(
                events.calls,
                [
                    "begin:Copying:Copied:the workspace".to_owned(),
                    format!("log:{}", work.base),
                    "end:".to_owned()
                ]
            );
            assert_eq!(work.cargo.features, config.cargo.features);
            assert_eq!(work.cargo.profile, config.cargo.profile);
            assert_eq!(work.cargo.test_args, config.cargo.test_args);
            assert_eq!(work.leak, leak);
            assert!(!work.settled.load(Ordering::Relaxed));
            assert!(!work.rustc_captures_ready.load(Ordering::Relaxed));
            assert!(!work.torn_down);
            assert!(work.nextest.is_none());
            assert!(work.test_environment.is_none());
            assert!(work.testing_journal.is_none());
            assert!(work.launch.get().is_none());
            assert!(work.harness_threads.get().is_none());
            assert_eq!(fs::read_to_string(work.root.join("src/lib.rs")).unwrap(), "pub fn subject() {}\n");
            let prepared_root = work.root.clone();
            work.teardown().unwrap();
            assert_eq!(prepared_root.exists(), leak);
        }
    }

    #[test]
    fn preparation_refuses_invalid_options_and_log_publication_before_copying_source_files() {
        let directory = crate::testing::workdir("exec-preparation-refusal-");
        let source = Utf8Path::from_path(directory.path()).unwrap();
        let mut config = Config::default();
        config.cargo.extra.push("--config=build.target='unmodeled'".into());
        let mut events = PreparationEvents::default();
        let failure = Workspace::prepare(source, &source.join("target"), &config, &mut events)
            .expect_err("unsupported Cargo configuration is refused before any preparation");
        assert!(failure.message().contains("is not supported"), "{failure}");
        assert!(events.calls.is_empty());
        config.cargo.extra.clear();
        events.refuse_log = true;
        let failure = Workspace::prepare(source, &source.join("target"), &config, &mut events)
            .expect_err("log publication failure must stop preparation without copying inputs");
        assert_eq!(failure.message(), "testing log was refused");
        assert_eq!(events.calls.len(), 2);
        assert_eq!(events.calls[0], "begin:Copying:Copied:the workspace");
        assert!(!scratch_tree(source, None).exists());
    }

    #[test]
    fn preparation_propagates_scratch_copy_runtime_and_manifest_failures_at_their_native_boundaries() {
        for boundary in ["scratch", "copy", "runtime", "manifest"] {
            let directory = crate::testing::workdir("exec-preparation-errors-");
            let source = Utf8Path::from_path(directory.path()).unwrap();
            crate::testing::write_project(
                source.as_std_path(),
                &[
                    ("Cargo.toml", "[package]\nname='prepared'\nversion='0.0.0'\n[workspace]\n"),
                    ("src/lib.rs", "pub fn subject() {}\n"),
                ],
            );
            let scratch = gamma_base(source, None);
            let mut locks = None;
            let expected = match boundary {
                "scratch" => {
                    fs::create_dir_all(scratch.parent().unwrap()).unwrap();
                    fs::write(&scratch, "not a directory").unwrap();
                    format!("could not create the scratch directory at `{scratch}`")
                }
                "copy" => {
                    fs::write(source.join(".ignore"), "ignored by searches only\n").unwrap();
                    #[cfg(windows)]
                    let name = { OsString::from_wide(&[0xd800]) };
                    #[cfg(unix)]
                    let name = { OsString::from_vec(vec![0xff]) };
                    fs::write(source.as_std_path().join(name), "invalid source name").unwrap();
                    "is not valid UTF-8".to_owned()
                }
                "runtime" => {
                    locks = Some(claim_cache(source, &source.join("target"), None).unwrap());
                    fs::write(scratch.join("rt"), "not a runtime directory").unwrap();
                    format!("could not create `{}`", scratch.join("rt").join("src"))
                }
                "manifest" => {
                    fs::write(source.join("Cargo.toml"), "[package\n").unwrap();
                    "could not parse".to_owned()
                }
                _ => unreachable!(),
            };
            let mut events = PreparationEvents::default();
            let failure = Workspace::prepare_with_locks(source, &source.join("target"), &Config::default(), &mut events, locks)
                .expect_err("each native boundary must retain its recoverable preparation failure");
            assert!(failure.message().contains(&expected), "{boundary}: {failure}");
            assert!(events.calls.iter().all(|call| !call.starts_with("end:")));
        }
    }

    #[test]
    fn testing_locators_reject_relative_foreign_and_unreadable_state_instead_of_searching_past_it() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = physical(Utf8Path::from_path(directory.path()).unwrap());
        crate::testing::with_cache_home_at(root.join("cache-home"), || {
            let base = root.join("state");
            fs::create_dir(&base).unwrap();
            mark_cache_owned_for_test(&root, &base);
            let locator = gamma_base(&root, None).join(CAMPAIGN_LOCATION);
            fs::create_dir_all(locator.parent().unwrap()).unwrap();
            let relative = base.strip_prefix(physical(Utf8Path::new("."))).unwrap();
            assert!(!relative.is_absolute());
            assert_eq!(cache_owner_root(relative), Some(root.clone()));
            fs::write(&locator, relative.as_str()).unwrap();
            let failure = locate_testing_state(&root, None).expect_err("even a valid owner does not authorize a relative locator");
            assert_eq!(
                failure.message(),
                format!("testing campaign location `{locator}` has an invalid owner; start a new campaign")
            );
            let foreign = root.join("foreign");
            fs::create_dir(&foreign).unwrap();
            mark_cache_owned_for_test(&foreign, &base);
            fs::write(&locator, base.as_str()).unwrap();
            let failure = locate_testing_state(&root, None).expect_err("an absolute locator must retain this workspace's ownership");
            assert_eq!(
                failure.message(),
                format!("testing campaign location `{locator}` has an invalid owner; start a new campaign")
            );
            fs::remove_file(&locator).unwrap();
            fs::create_dir(&locator).unwrap();
            let failure = locate_testing_state(&root, None).expect_err("a directory in place of the locator is not NotFound");
            assert_eq!(failure.message(), format!("could not read testing campaign location `{locator}`"));
        });
    }

    #[test]
    fn claiming_testing_state_rechecks_ownership_after_locking_and_releases_failed_claims() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = physical(Utf8Path::from_path(directory.path()).unwrap());
        crate::testing::with_cache_home_at(root.join("cache-home"), || {
            let mut resolutions = 0;
            let failure = claim_testing_state_with(&root, None, |_, _| {
                resolutions += 1;
                let owner = if resolutions == 1 { root.clone() } else { root.join("changed") };
                Ok((owner, root.join("state")))
            })
            .expect_err("a changed root cannot be authorized by a lock on the first root");
            assert_eq!(resolutions, 2);
            assert_eq!(
                failure.message(),
                "the testing campaign changed while acquiring its lock; retry `--resume-testing`"
            );
            let lock = claim_workspace(&root).expect("the rejected recheck must release its workspace lock");
            drop(lock);
            let failure = claim_testing_state_with(&root, None, |_, _| Err(error!("initial resolution failed")))
                .expect_err("initial location errors are not panics");
            assert_eq!(failure.message(), "initial resolution failed");
            let mut resolutions = 0;
            let failure = claim_testing_state_with(&root, None, |_, _| {
                resolutions += 1;
                if resolutions == 1 {
                    Ok((root.clone(), root.join("state")))
                } else {
                    Err(error!("locked resolution failed"))
                }
            })
            .expect_err("the post-lock location error must be returned");
            assert_eq!(failure.message(), "locked resolution failed");
            assert_eq!(resolutions, 2);
        });
    }

    #[test]
    fn owned_cache_cleanup_preserves_only_the_requested_lock_and_owner() {
        for preserve_lock in [false, true] {
            let directory = tempfile::tempdir_in(".").unwrap();
            let root = Utf8PathBuf::from_path_buf(fs::canonicalize(directory.path()).unwrap()).unwrap();
            let base = root.join("cache");
            fs::create_dir(&base).unwrap();
            mark_cache_owned_for_test(&root, &base);
            fs::write(base.join("lock"), "lock bytes").unwrap();
            fs::create_dir_all(base.join("data/deep")).unwrap();
            fs::write(base.join("data/deep/file"), "stale").unwrap();
            fs::write(base.join("artifact"), "stale").unwrap();
            assert!(clean_owned_cache(&root, &base, preserve_lock).unwrap());
            assert_eq!(fs::read_to_string(base.join(CACHE_OWNER)).unwrap(), root.as_str());
            assert_eq!(base.join("lock").exists(), preserve_lock);
            assert!(!base.join("artifact").exists());
            assert!(!base.join("data").exists());
            assert!(!clean_owned_cache(&root, &base, preserve_lock).unwrap());
            if preserve_lock {
                assert_eq!(fs::read(base.join("lock")).unwrap(), b"lock bytes");
            }
        }
    }

    #[test]
    fn cache_cleanup_handles_each_independent_tree_and_preserves_only_the_workspace_lock() {
        for (has_scratch, has_campaign) in [(false, false), (true, false), (false, true), (true, true)] {
            let directory = tempfile::tempdir_in(".").unwrap();
            let root = physical(Utf8Path::from_path(directory.path()).unwrap());
            let scratch = root.join("scratch");
            let campaign = root.join("campaign");
            for (present, base) in [(has_scratch, &scratch), (has_campaign, &campaign)] {
                if present {
                    fs::create_dir(base).unwrap();
                    mark_cache_owned_for_test(&root, base);
                    fs::write(base.join("data"), "stale").unwrap();
                    fs::write(base.join("lock"), "lock").unwrap();
                }
            }
            let mut claims = 0;
            let removed = clean_cache_paths(&root, &scratch, &campaign, || {
                claims += 1;
                File::create(root.join("held-workspace-lock")).map_err(crate::error::Error::from)
            })
            .unwrap();
            assert_eq!(removed, has_scratch || has_campaign);
            assert_eq!(claims, usize::from(has_scratch || has_campaign));
            assert_eq!(scratch.exists(), has_scratch);
            assert_eq!(campaign.exists(), has_campaign);
            if has_scratch {
                assert_eq!(fs::read(scratch.join("lock")).unwrap(), b"lock");
                assert!(!scratch.join("data").exists());
                assert_eq!(cache_owner_root(&scratch), Some(root.clone()));
            }
            if has_campaign {
                assert!(!campaign.join("lock").exists());
                assert!(!campaign.join("data").exists());
                assert_eq!(cache_owner_root(&campaign), Some(root.clone()));
            }
        }
    }

    #[test]
    fn cache_cleanup_does_not_clean_an_alias_twice_or_hide_either_tree_ownership_error() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = physical(Utf8Path::from_path(directory.path()).unwrap());
        let scratch = root.join("scratch");
        fs::create_dir(&scratch).unwrap();
        mark_cache_owned_for_test(&root, &scratch);
        fs::write(scratch.join("lock"), "keep").unwrap();
        fs::write(scratch.join("data"), "remove").unwrap();
        let claim = || File::create(root.join("held-workspace-lock")).map_err(crate::error::Error::from);
        assert!(clean_cache_paths(&root, &scratch, &scratch.join("."), claim).unwrap());
        assert_eq!(fs::read(scratch.join("lock")).unwrap(), b"keep");
        let campaign = root.join("campaign");
        fs::create_dir(&campaign).unwrap();
        fs::write(campaign.join("unowned"), "must remain").unwrap();
        let failure =
            clean_cache_paths(&root, &scratch, &campaign, claim).expect_err("campaign cleanup must not silently adopt foreign data");
        assert!(failure.message().contains("not marked as cargo-gamma state"), "{failure}");
        assert_eq!(fs::read(campaign.join("unowned")).unwrap(), b"must remain");
        fs::remove_file(scratch.join(CACHE_OWNER)).unwrap();
        fs::write(scratch.join("unowned"), "must remain").unwrap();
        let failure = clean_cache_paths(&root, &scratch, &root.join("missing"), claim)
            .expect_err("scratch ownership failure must propagate before cleanup");
        assert!(failure.message().contains("not marked as cargo-gamma state"), "{failure}");
        assert_eq!(fs::read(scratch.join("lock")).unwrap(), b"keep");
    }

    #[test]
    fn wrapper_identity_requires_a_matching_name_or_the_same_existing_file() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8PathBuf::from_path_buf(fs::canonicalize(directory.path()).unwrap()).unwrap();
        let wrapper = root.join("wrapper.exe");
        fs::write(&wrapper, "wrapper").unwrap();
        assert!(wrapper_is_self(OsStr::new("wrapper.exe"), &wrapper, &root));
        assert!(wrapper_is_self(wrapper.as_os_str(), &wrapper, &root));
        assert!(wrapper_is_self(OsStr::new("./wrapper.exe"), &wrapper, &root));
        assert!(!wrapper_is_self(OsStr::new("different.exe"), &wrapper, &root));
        assert!(!wrapper_is_self(OsStr::new("missing/wrapper.exe"), &wrapper, &root));
        assert!(executable_names_equal("wrapper", "wrapper"));
        assert_eq!(executable_names_equal("Wrapper", "wrapper"), cfg!(windows));
        assert!(!executable_names_equal("different", "wrapper"));
    }

    #[test]
    fn extending_flags_pins_empty_and_encoded_separators_without_ambient_environment() {
        for (inherited, separator, expected) in [
            ("", " ", " --cap-lints=allow"),
            ("-Dwarnings", " ", "-Dwarnings --cap-lints=allow"),
            ("--cfg=with space", "\u{1f}", "--cfg=with space\u{1f}--cap-lints=allow"),
        ] {
            let mut command = Command::new("not-executed");
            extend(&mut command, "FLAGS", inherited.into(), separator);
            assert_eq!(
                command.get_envs().find(|(key, _)| *key == "FLAGS").unwrap().1,
                Some(OsStr::new(expected))
            );
        }
    }

    #[test]
    fn ambient_flag_precedence_and_target_name_filters_use_exact_environment_spellings() {
        for encoded in [None, Some(""), Some("-D\u{1f}warnings")] {
            for plain in [None, Some(""), Some("-D warnings")] {
                let mut environment = std::collections::BTreeMap::<OsString, OsString>::from_iter([
                    ("CARGO_TARGET_ALPHA_RUSTFLAGS".into(), "--cfg alpha".into()),
                    ("CARGO_TARGET_BETA_RUSTFLAGS".into(), "".into()),
                    ("CARGO_TARGET_ALPHA_RUNNER".into(), "runner".into()),
                    ("NOT_CARGO_TARGET_ALPHA_RUSTFLAGS".into(), "other".into()),
                    ("_RUSTFLAGS_CARGO_TARGET_".into(), "reverse".into()),
                    ("CARGO_BUILD_RUSTFLAGS".into(), "-C opt-level=2".into()),
                ]);
                if let Some(value) = encoded {
                    let _ = environment.insert("CARGO_ENCODED_RUSTFLAGS".into(), value.into());
                }
                if let Some(value) = plain {
                    let _ = environment.insert("RUSTFLAGS".into(), value.into());
                }
                let mut command = Command::new("unused");
                cap_ambient_rustflags_from(
                    &mut command,
                    |name| environment.get(OsStr::new(name)).cloned(),
                    || environment.clone(),
                );
                let actual = command
                    .get_envs()
                    .map(|(key, value)| (key.to_str().unwrap().to_owned(), value.unwrap().to_str().unwrap().to_owned()))
                    .collect::<std::collections::BTreeMap<_, _>>();
                let expected = if let Some(value) = encoded {
                    std::collections::BTreeMap::from([("CARGO_ENCODED_RUSTFLAGS".into(), format!("{value}\u{1f}--cap-lints=allow"))])
                } else if let Some(value) = plain {
                    std::collections::BTreeMap::from([("RUSTFLAGS".into(), format!("{value} --cap-lints=allow"))])
                } else {
                    std::collections::BTreeMap::from([
                        ("CARGO_TARGET_ALPHA_RUSTFLAGS".into(), "--cfg alpha --cap-lints=allow".into()),
                        ("CARGO_TARGET_BETA_RUSTFLAGS".into(), " --cap-lints=allow".into()),
                        ("CARGO_BUILD_RUSTFLAGS".into(), "-C opt-level=2 --cap-lints=allow".into()),
                    ])
                };
                assert_eq!(actual, expected);
            }
        }
    }

    #[test]
    fn production_campaign_validation_requires_every_component_and_the_resolved_target() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let root = Utf8PathBuf::from_path_buf(fs::canonicalize(directory.path()).unwrap()).unwrap();
        let target = root.join("artifacts");
        let identity = workspace_identity(&root);
        let valid = target.join("cargo-gamma/cache").join(&identity);
        validate_production_campaign_base(&root, &target, &valid).expect("the cache namespace and identity match the resolved target");
        for invalid in [
            target.join("cargo-gamma/cache/wrong-identity"),
            target.join("cargo-gamma/wrong-cache").join(&identity),
            target.join("wrong-namespace/cache").join(&identity),
            root.join("other-target/cargo-gamma/cache").join(&identity),
            Utf8PathBuf::new(),
            Utf8PathBuf::from(&identity),
        ] {
            let error = validate_production_campaign_base(&root, &target, &invalid).unwrap_err();
            assert!(error.message().starts_with("cargo-gamma refused the malformed campaign cache path"));
            assert!(!invalid.exists());
        }
        let failure = validate_production_campaign_base(&root, &root.join("different-target"), &valid)
            .expect_err("a campaign base must belong to the resolved target");
        assert!(failure.message().contains(valid.as_str()));
    }

    /// A workspace over no real directory, for tests that only inspect the `Command` it builds.
    ///
    /// Leaked rather than torn down: nothing was ever created, so nothing should be removed.
    fn unsettled_default() -> Workspace {
        let root = Utf8PathBuf::from(if cfg!(windows) {
            "C:/workspace-policy"
        } else {
            "/workspace-policy"
        });
        Workspace::adopt(root.clone(), root.join("target"))
    }

    /// `CARGO_ENCODED_RUSTFLAGS` uses a unit separator rather than a space, and cargo prefers it
    /// over the plain form; extending it in place rather than replacing it keeps whatever flags the
    /// user's own environment configured instead of silently dropping them from the instrumented
    /// build.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn an_ambient_encoded_rustflags_is_extended_rather_than_replaced() {
        if env::var_os(crate::exec::UNDER_GAMMA_VAR).is_some() {
            return;
        }

        let child = run_child(
            "cargo_flags",
            &[("CARGO_ENCODED_RUSTFLAGS", Some("--cfg\u{1f}loom")), ("RUSTFLAGS", None)],
        );
        let value = &child["encoded"];

        assert!(value.contains("--cfg\u{1f}loom"), "{value}");
        assert!(value.contains(CAP_LINTS), "{value}");
    }

    /// The plain `RUSTFLAGS` form is only read when the encoded one is absent, but it must be
    /// extended the same way: replacing it would silently drop whatever the caller's environment
    /// configured, changing what the instrumented tree compiles to.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn an_ambient_rustflags_is_extended_rather_than_replaced() {
        if env::var_os(crate::exec::UNDER_GAMMA_VAR).is_some() {
            return;
        }

        let child = run_child(
            "cargo_flags",
            &[("CARGO_ENCODED_RUSTFLAGS", None), ("RUSTFLAGS", Some("--cfg loom"))],
        );
        let value = &child["rustflags"];

        assert!(value.contains("--cfg loom"), "{value}");
        assert!(value.contains(CAP_LINTS), "{value}");
    }

    /// `CARGO_BUILD_RUSTFLAGS` replaces the `[build] rustflags` array `cap_lints` writes into the
    /// copied tree, so the cap has to be added to the variable as well.
    ///
    /// Left out, an ambient `-D warnings` — routine in CI images and `direnv` setups — compiles the
    /// instrumented tree under deny-by-default: guard-induced warnings become errors, the mutants
    /// that produced them are withdrawn as unviable, and the score is computed over a silently
    /// smaller denominator.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn an_ambient_build_rustflags_carries_the_cap() {
        if env::var_os(crate::exec::UNDER_GAMMA_VAR).is_some() {
            return;
        }

        let child = run_child(
            "cargo_flags",
            &[
                ("CARGO_ENCODED_RUSTFLAGS", None),
                ("RUSTFLAGS", None),
                ("CARGO_BUILD_RUSTFLAGS", Some("-D warnings")),
            ],
        );
        let value = &child["build"];

        assert!(value.contains("-D warnings"), "{value}");
        assert!(value.contains(CAP_LINTS), "{value}");
    }

    /// The same for the target-table spelling, which outranks the build one.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn an_ambient_target_rustflags_carries_the_cap() {
        if env::var_os(crate::exec::UNDER_GAMMA_VAR).is_some() {
            return;
        }

        let child = run_child(
            "cargo_flags",
            &[
                ("CARGO_ENCODED_RUSTFLAGS", None),
                ("RUSTFLAGS", None),
                (TARGET_RUSTFLAGS_VAR, Some("-D warnings")),
            ],
        );
        let value = &child["target"];

        assert!(value.contains("-D warnings"), "{value}");
        assert!(value.contains(CAP_LINTS), "{value}");
    }

    /// A global spelling beats every configured key, so it is the only one extended.
    ///
    /// Extending the lower levels as well would be harmless to the build — cargo never reads them
    /// once a global one is set — but it would put the tool's flag into variables the child process
    /// passes on to anything it in turn runs.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn a_global_rustflags_leaves_the_lower_spellings_alone() {
        if env::var_os(crate::exec::UNDER_GAMMA_VAR).is_some() {
            return;
        }

        let child = run_child(
            "cargo_flags",
            &[
                ("CARGO_ENCODED_RUSTFLAGS", None),
                ("RUSTFLAGS", Some("--cfg loom")),
                ("CARGO_BUILD_RUSTFLAGS", Some("-D warnings")),
            ],
        );

        assert!(child["rustflags"].contains(CAP_LINTS), "{}", child["rustflags"]);
        assert_eq!(
            child["build"], PAYLOAD_MISSING,
            "cargo will not read this one, so it must be left as it was"
        );
    }

    #[test]
    fn nextest_inventories_the_profile_gamma_built() {
        let mut work = unsettled_default();
        work.cargo.features = vec!["--all-features".to_owned()];
        work.cargo.profile = Some("mutants".to_owned());
        let binaries = [TestBinary {
            package: "subject".to_owned(),
            package_id: "path+file:///tmp/subject#subject@0.1.0".to_owned(),
            ..crate::testing::test_binary("/tmp/subject")
        }];

        let command = work.nextest_list_command(&binaries);
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();

        assert_eq!(
            args,
            vec![
                "nextest",
                "list",
                "--list-type",
                "binaries-only",
                "--message-format",
                "json",
                "--package",
                "path+file:///tmp/subject#subject@0.1.0",
                "--all-features",
                "--cargo-profile",
                "mutants",
            ]
        );
    }

    /// The default cache directory's name is a cross-release contract, not a private detail.
    ///
    /// Every command that can publish source or configuration changes for a workspace shares one
    /// advisory lock, and that lock lives under this name. Two binaries that derived different
    /// names for one workspace would each take a lock nobody else holds and rewrite the same tree
    /// at the same time, which is precisely the situation the lock exists to prevent — and nothing
    /// would report contention, because there is none to report. So the digest is pinned here
    /// against a literal, and any change to the algorithm has to be a deliberate one made with
    /// that consequence in view.
    #[test]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn the_default_cache_directory_name_is_pinned_to_this_crate() {
        assert_eq!(digest_workspace_path(Utf8Path::new("/workspace")), "155d8208f4c61a79");
        assert_eq!(digest_workspace_path(Utf8Path::new("/workspace/one")), "85dacdefd093e7c1");

        // Distinct roots get distinct directories, which is the whole reason to hash at all.
        assert_ne!(
            digest_workspace_path(Utf8Path::new("/workspace/one")),
            digest_workspace_path(Utf8Path::new("/workspace/two"))
        );

        // And it is the name the default base actually uses.
        let expected = workspace_identity(&absolute(Utf8Path::new("/workspace")));
        assert!(
            gamma_base(Utf8Path::new("/workspace"), None).as_str().ends_with(&expected),
            "{}",
            gamma_base(Utf8Path::new("/workspace"), None)
        );
    }

    #[test]
    fn campaign_state_uses_the_resolved_target_and_workspace_identity() {
        let root = absolute(Utf8Path::new("/workspace"));
        let target = absolute(Utf8Path::new("/shared-target"));
        let identity = workspace_identity(&root);
        let base = production_campaign_base(&root, &target);

        assert_eq!(base, target.join("cargo-gamma/cache").join(identity));
        assert_ne!(base, production_gamma_base(&root), "the former external state is not adopted");
        assert_eq!(campaign_base(&root, &target, None), default_campaign_base(&root, &target));
    }

    #[test]
    fn campaign_state_walks_from_a_member_to_the_persisted_workspace_locator() {
        let directory = crate::testing::workdir("workspace-campaign-member-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("workspace")).expect("workspace path is UTF-8");
        let member = root.join("member");
        let base = root.join("campaign");
        let cache_home = Utf8PathBuf::from_path_buf(directory.path().join("cache-home")).expect("cache path is UTF-8");
        fs::create_dir_all(&member).expect("workspace member");
        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"member\"]\nresolver = \"3\"\n").expect("workspace manifest");
        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .expect("member manifest");
        fs::create_dir_all(member.join("src")).expect("member source");
        fs::write(member.join("src/lib.rs"), "").expect("member library");
        fs::create_dir_all(&base).expect("campaign base");
        fs::write(base.join(CACHE_OWNER), physical(&root).as_str()).expect("campaign owner");

        crate::testing::with_cache_home_at(cache_home, || {
            assert!(remember_campaign_base(&root, &base));

            let (resolved, located) = campaign_state_from(&member, None).expect("persisted campaign state");

            assert_eq!(resolved, physical(&root));
            assert_eq!(located, base);
        });
    }

    #[test]
    fn campaign_state_ignores_a_locator_for_a_former_inner_workspace() {
        let directory = crate::testing::workdir("workspace-campaign-stale-inner-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("workspace")).expect("workspace path is UTF-8");
        let member = root.join("member");
        let old_base = member.join("old-campaign");
        let cache_home = Utf8PathBuf::from_path_buf(directory.path().join("cache-home")).expect("cache path is UTF-8");
        fs::create_dir_all(member.join("src")).expect("member source");
        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .expect("member manifest");
        fs::write(member.join("src/lib.rs"), "").expect("member library");
        fs::create_dir_all(&old_base).expect("former campaign base");
        fs::write(old_base.join(CACHE_OWNER), physical(&member).as_str()).expect("former campaign owner");

        crate::testing::with_cache_home_at(cache_home, || {
            assert!(remember_campaign_base(&member, &old_base));
            fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"member\"]\nresolver = \"3\"\n")
                .expect("outer workspace manifest");

            let (resolved, located) = campaign_state_from(&member, None).expect("current campaign state");

            assert_eq!(resolved, physical(&root));
            assert_ne!(located, old_base);
        });
    }

    #[test]
    fn campaign_state_does_not_adopt_a_foreign_locator_as_fallback_state() {
        let directory = crate::testing::workdir("workspace-campaign-foreign-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("workspace")).expect("workspace path is UTF-8");
        let member = root.join("member");
        let foreign_root = Utf8PathBuf::from_path_buf(directory.path().join("foreign")).expect("foreign path is UTF-8");
        let foreign_base = foreign_root.join("campaign");
        let cache_home = Utf8PathBuf::from_path_buf(directory.path().join("cache-home")).expect("cache path is UTF-8");
        fs::create_dir_all(&member).expect("workspace member");
        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"member\"]\nresolver = \"3\"\n").expect("workspace manifest");
        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .expect("member manifest");
        fs::create_dir_all(member.join("src")).expect("member source");
        fs::write(member.join("src/lib.rs"), "").expect("member library");
        fs::create_dir_all(&foreign_base).expect("foreign campaign");
        fs::write(foreign_base.join(CACHE_OWNER), physical(&foreign_root).as_str()).expect("foreign owner");

        crate::testing::with_cache_home_at(cache_home, || {
            let locator = gamma_base(&root, None).join(CAMPAIGN_LOCATION);
            fs::create_dir_all(locator.parent().expect("locator parent")).expect("locator directory");
            fs::write(&locator, foreign_base.as_str()).expect("foreign locator");

            let error = campaign_state_from(&member, None).expect_err("foreign locator cannot become fallback state");

            assert!(error.to_string().contains("is not owned by the selected workspace"), "{error}");
        });
    }

    #[test]
    fn explicit_campaign_cache_rejects_a_foreign_owner() {
        let directory = crate::testing::workdir("workspace-campaign-explicit-foreign-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("workspace")).expect("workspace path is UTF-8");
        let foreign = Utf8PathBuf::from_path_buf(directory.path().join("foreign")).expect("foreign path is UTF-8");
        crate::testing::write_project(
            root.as_std_path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
                ),
                ("src/lib.rs", ""),
            ],
        );
        fs::create_dir_all(&foreign).expect("foreign cache");
        fs::write(foreign.join(CACHE_OWNER), physical(&foreign).as_str()).expect("foreign owner");

        let error = campaign_state_from(&root, Some(&foreign)).expect_err("foreign explicit cache");

        assert!(error.to_string().contains("is not owned by the selected workspace"), "{error}");
    }

    #[test]
    fn metadata_fallback_rejects_campaign_state_owned_by_another_workspace() {
        let directory = crate::testing::workdir("workspace-campaign-derived-foreign-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("workspace")).expect("workspace path is UTF-8");
        let target = root.join("target");
        let foreign = Utf8PathBuf::from_path_buf(directory.path().join("foreign")).expect("foreign path is UTF-8");
        crate::testing::write_project(
            root.as_std_path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
                ),
                ("src/lib.rs", ""),
            ],
        );
        let base = campaign_base(&root, &target, None);
        fs::create_dir_all(&base).expect("campaign base");
        fs::write(base.join(CACHE_OWNER), physical(&foreign).as_str()).expect("foreign owner");

        let error = campaign_state_from(&root, None).expect_err("foreign derived campaign state");

        assert!(error.to_string().contains("is not owned by the selected workspace"), "{error}");
    }

    #[test]
    fn metadata_fallback_uses_cargos_resolved_target_directory() {
        let directory = crate::testing::workdir("workspace-campaign-target-");
        let root = Utf8PathBuf::from_path_buf(directory.path().join("workspace")).expect("workspace path is UTF-8");
        crate::testing::write_project(
            root.as_std_path(),
            &[
                (
                    "Cargo.toml",
                    "[workspace]\n\n[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
                ),
                ("src/lib.rs", ""),
                (".cargo/config.toml", "[build]\ntarget-dir = \"configured-target\"\n"),
            ],
        );
        let mut command = cargo_metadata::MetadataCommand::new();
        let metadata = command.current_dir(&root).no_deps().exec().expect("cargo metadata");
        let expected_target = physical(Utf8Path::new(metadata.target_directory.as_str()));
        let (_, resolved_target) = metadata_workspace(&root).expect("metadata workspace");

        let (resolved, located) = campaign_state_from(&root, None).expect("metadata campaign state");

        assert_eq!(resolved_target, expected_target);
        assert_eq!(resolved, physical(&root));
        assert_eq!(physical(&located), physical(&campaign_base(&root, &expected_target, None)));
    }

    #[test]
    fn a_shared_target_keeps_workspace_campaigns_separate() {
        let target = absolute(Utf8Path::new("/shared-target"));
        let first = production_campaign_base(Utf8Path::new("/workspace/one"), &target);
        let second = production_campaign_base(Utf8Path::new("/workspace/two"), &target);
        let namespace = target.join("cargo-gamma/cache");

        assert_ne!(first, second);
        assert_eq!(first.parent(), second.parent());
        assert_eq!(first.parent(), Some(namespace.as_path()));
    }

    #[test]
    fn an_adopted_workspace_uses_the_cargo_target_parent_as_its_campaign_base() {
        let work = Workspace::adopt(Utf8PathBuf::from("/scratch/workspace"), Utf8PathBuf::from("/campaign/target"));

        assert_eq!(work.base(), Utf8Path::new("/campaign"));
    }

    #[test]
    fn a_workspace_local_campaign_base_keeps_a_prunable_spelling() {
        let directory = crate::testing::workdir("workspace-local-campaign-");
        let source = absolute(&Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("the workspace path is UTF-8"));
        let target = source.join("target");
        let scratch = absolute(Utf8Path::new("/external/cache"));

        fs::create_dir_all(&target).expect("the workspace target");
        let base = production_campaign_base(&source, &target);

        assert!(prunes(&source, &base), "source={source} base={base}");
        ensure_copy_layout_terminates(&source, &scratch, &base).expect("the workspace target is excluded from the copy");
    }

    #[test]
    fn an_explicit_cache_keeps_scratch_state_and_cargo_artifacts_together() {
        let root = absolute(Utf8Path::new("/workspace"));
        let target = absolute(Utf8Path::new("/target"));
        let explicit = absolute(Utf8Path::new("/chosen/cache"));

        assert_eq!(gamma_base(&root, Some(&explicit)), explicit);
        assert_eq!(campaign_base(&root, &target, Some(&explicit)), explicit);
    }

    #[test]
    fn only_absolute_environment_cache_homes_define_the_lock_domain() {
        let absolute = absolute(Utf8Path::new("."));

        assert_eq!(absolute_cache_home(absolute.clone().into()), Some(absolute));
        assert_eq!(absolute_cache_home(OsString::from("relative/cache")), None);
        assert_eq!(absolute_cache_home(OsString::new()), None);
    }

    #[test]
    fn the_xdg_cache_variable_and_tool_directory_define_the_default_home() {
        let root = absolute(Utf8Path::new("."));
        let cache = root.join("xdg");
        let mut requested = Vec::new();
        let actual = default_cache_home_with(&root, |name| {
            requested.push(name.to_owned());
            (name == "XDG_CACHE_HOME").then(|| cache.clone().into())
        });

        assert_eq!(requested, ["XDG_CACHE_HOME"]);
        assert_eq!(actual, cache.join("cargo-gamma"));
    }

    /// A broken derivation is stopped before it can claim the cache home or namespace itself.
    #[test]
    fn malformed_default_cache_paths_are_refused_before_any_state_is_written() {
        let directory = crate::testing::workdir("malformed-default-cache-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is UTF-8");
        let source = root.join("source");
        let home = root.join("platform-cache");
        let namespace = home.join("cargo-gamma");
        let wrong_identity = namespace.join("not-the-workspace-identity");

        fs::create_dir_all(&source).expect("workspace");
        fs::create_dir_all(&namespace).expect("cache namespace");

        for malformed in [&home, &namespace, &wrong_identity] {
            let failure = claim_workspace_at(&source, malformed).expect_err("a malformed default cache must be refused");

            assert!(failure.to_string().contains("malformed default cache path"), "{failure}");
            assert!(!malformed.join("lock").exists(), "a lock escaped to `{malformed}`");
            assert!(!malformed.join(CACHE_OWNER).exists(), "an owner marker escaped to `{malformed}`");
        }
    }

    #[test]
    fn relative_scratch_paths_are_made_absolute() {
        let path = absolute(Utf8Path::new("nested/cache"));

        assert!(path.is_absolute(), "{path}");
        assert!(path.ends_with("nested/cache"), "{path}");
    }

    /// Filesystem aliases of one Windows workspace share one cache and lock identity.
    #[cfg(windows)]
    #[test]
    fn case_aliases_of_one_workspace_share_the_default_cache_identity() {
        let directory = crate::testing::workdir("workspace-case-identity");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is UTF-8");
        let alias = Utf8PathBuf::from(root.as_str().to_uppercase());
        let target = root.join("target");
        let target_alias = Utf8PathBuf::from(target.as_str().to_uppercase());
        fs::create_dir(&target).expect("the physical target");

        assert_eq!(workspace_identity(&root), workspace_identity(&alias));
        assert_eq!(production_gamma_base(&root), production_gamma_base(&alias));
        assert_eq!(
            physical(&production_campaign_base(&root, &target)),
            physical(&production_campaign_base(&alias, &target_alias))
        );
    }

    /// A truncated digest can collide, so the directory says which workspace it belongs to and a
    /// second one is refused rather than handed the first one's lock domain and scratch tree.
    #[test]
    fn a_default_cache_directory_claimed_by_another_workspace_is_refused() {
        let directory = crate::testing::workdir("default-owner");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let base = root.join("cache");
        let mine = root.join("mine");
        let theirs = root.join("theirs");

        fs::create_dir_all(base.as_std_path()).expect("an empty cache");
        fs::create_dir_all(mine.as_std_path()).expect("one workspace root");
        fs::create_dir_all(theirs.as_std_path()).expect("another workspace root");

        // An unclaimed directory is adopted, and says so afterwards.
        validate_cache_owner(&mine, &base, CacheKind::Default).expect("an unclaimed default cache is adopted");
        assert_eq!(
            fs::read_to_string(base.join(CACHE_OWNER).as_std_path()).expect("the owner marker"),
            physical(&mine).as_str()
        );

        // The owner may keep using it.
        validate_cache_owner(&mine, &base, CacheKind::Default).expect("the owning workspace reuses its cache");

        let failure = validate_cache_owner(&theirs, &base, CacheKind::Default).expect_err("a colliding workspace must be refused");

        assert!(failure.to_string().contains(physical(&mine).as_str()), "{failure}");
        assert!(failure.to_string().contains("--cache-dir"), "{failure}");
        assert!(failure.is_usage(), "{failure}");
    }

    #[test]
    fn an_unmarked_populated_cache_is_refused_for_default_and_redirected_paths() {
        let directory = crate::testing::workdir("unmarked-cache");
        let base = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let source = base.join("source");

        fs::create_dir_all(source.as_std_path()).expect("a workspace root");
        fs::create_dir_all(base.join("target").as_std_path()).expect("cached build output");

        for kind in [CacheKind::Redirected, CacheKind::Default] {
            let failure = validate_cache_owner(&source, &base, kind).expect_err("existing contents are unowned");

            assert!(failure.is_usage(), "{failure}");
            assert!(!base.join(CACHE_OWNER).exists(), "nothing may be claimed on the refused path");
        }
    }

    #[test]
    fn scratch_tree_is_derived_from_the_same_base_as_prepare() {
        // Callers report this path after deliberately leaking the workspace, so it has to match
        // the tree `prepare` would have created.
        assert_eq!(
            scratch_tree(Utf8Path::new("/workspace"), None),
            gamma_base(Utf8Path::new("/workspace"), None).join("workspace")
        );
        assert_eq!(
            scratch_tree(Utf8Path::new("/workspace"), Some(Utf8Path::new("/scratch"))),
            gamma_base(Utf8Path::new("/workspace"), Some(Utf8Path::new("/scratch"))).join("workspace")
        );
    }

    #[test]
    fn the_default_scratch_tree_cannot_rediscover_workspace_cargo_configuration() {
        let root = Utf8Path::new("/workspace");
        let base = production_gamma_base(root);

        assert!(!base.starts_with(root), "{base}");
        assert!(
            !base.ancestors().any(|ancestor| ancestor == root.join(".cargo")),
            "the real workspace configuration remains in Cargo's scratch ancestor chain: {base}"
        );
    }

    #[test]
    fn array_rustflags_from_workspace_config_reach_scratch_cargo_once() {
        let directory = crate::testing::workdir("scratch-config-once");
        let cache_directory = tempfile::tempdir().expect("a cache directory");
        let source = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf8");
        let cache = Utf8PathBuf::from_path_buf(cache_directory.path().to_path_buf()).expect("utf8");
        let capture = source.join("captured-flags");

        crate::testing::write_project(
            source.as_std_path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"scratch-config-once\"\nversion = \"0.0.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\n[workspace]\n",
                ),
                ("src/lib.rs", ""),
                (
                    "build.rs",
                    "fn main() { std::fs::write(std::env::var(\"GAMMA_CAPTURE\").unwrap(), std::env::var(\"CARGO_ENCODED_RUSTFLAGS\").unwrap()).unwrap(); }\n",
                ),
                (".cargo/config.toml", "[build]\nrustflags = [\"--cfg\", \"gamma_once\"]\n"),
            ],
        );
        let mut events = crate::testing::Recorder::default();
        let work = crate::testing::with_cache_home_at(cache, || {
            Workspace::prepare(&source, &source.join("target"), &Config::default(), &mut events)
        })
        .expect("prepare");
        let mut command = work.cargo();
        let status = command
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_BUILD_RUSTFLAGS")
            .env("GAMMA_CAPTURE", capture.as_std_path())
            .arg("check")
            .status()
            .expect("cargo check");

        assert!(status.success(), "{status}");
        let flags = fs::read_to_string(capture.as_std_path()).expect("captured flags");
        assert_eq!(flags.matches("gamma_once").count(), 1, "{flags}");
    }

    /// A relative `--cache-dir` reaches the copy as the path the user typed, and the copy prunes
    /// its own destination by comparing that against the absolute paths it walks — which never
    /// match, so the copy descends into what it is writing. Absolutising the base is what stops it.
    #[test]
    fn a_relative_cache_directory_is_resolved_against_the_current_directory() {
        let cwd = Utf8PathBuf::from_path_buf(env::current_dir().unwrap()).unwrap();
        let base = gamma_base(Utf8Path::new("/workspace"), Some(Utf8Path::new("scratch/here")));

        assert_eq!(base, cwd.join("scratch/here"));
        assert!(base.is_absolute(), "{base}");

        // The tree, the build directory and the runtime all hang off the base, so absolutising it
        // once makes every one of them absolute.
        assert!(scratch_tree(Utf8Path::new("/workspace"), Some(Utf8Path::new("scratch/here"))).is_absolute());

        // A workspace reached by a relative path gets the same treatment.
        assert_eq!(gamma_base(Utf8Path::new("."), None), gamma_base(&cwd, None));
    }

    /// An absolute cache directory outside the workspace is the arrangement everything else is
    /// built on, and must come back exactly as given — a run whose cache path moved between two
    /// invocations rebuilds from cold.
    #[test]
    fn an_absolute_cache_directory_outside_the_workspace_is_left_alone() {
        let base = gamma_base(Utf8Path::new("/workspace"), Some(Utf8Path::new("/elsewhere/scratch")));

        assert_eq!(base, absolute(Utf8Path::new("/elsewhere/scratch")));
        ensure_copy_terminates(Utf8Path::new("/workspace"), &base).expect("a scratch directory outside the workspace is fine");

        // An explicitly selected path inside the workspace is fine as long as the copy prunes it.
        let inside = production_gamma_base(Utf8Path::new("/workspace"));

        ensure_copy_terminates(Utf8Path::new("/workspace"), &inside).expect("the default base is outside the copy");
        assert!(!inside.starts_with("/workspace"), "{inside}");
    }

    /// The walk never tests its own root against the exclusion, so a base that is the workspace
    /// itself would have the copy copying its own output. Producing a tree that contains a copy of
    /// itself measures mutants in the wrong files, so the run stops instead.
    #[test]
    fn a_scratch_directory_the_copy_cannot_prune_is_refused() {
        let source = absolute(Utf8Path::new("/workspace/gamma"));
        let base = gamma_base(&source, Some(&source));

        assert_eq!(base, source);
        assert!(!prunes(&source, &base));

        let failure = ensure_copy_terminates(&source, &base).expect_err("an unprunable base must be refused");

        assert!(failure.is_usage(), "{failure}");
        assert!(failure.to_string().contains(source.as_str()), "{failure}");
        assert!(failure.to_string().contains("--cache-dir"), "{failure}");
    }

    /// A scratch directory that reaches back into the workspace through a link is refused.
    ///
    /// The copy's exclusion is a path comparison against the paths the walk produces, and the walk
    /// produces the physical spelling. A base named outside the workspace therefore never matches,
    /// however deep inside the workspace it really is — so the copy descends into its own output
    /// and writes until the disk is gone. A lexical check answers this one wrongly, which is the
    /// whole reason the check resolves the links first.
    #[test]
    #[cfg(unix)]
    fn a_scratch_directory_linked_back_into_the_workspace_is_refused() {
        let directory = crate::testing::workdir("scratch-linked");
        let root = Utf8Path::from_path(directory.path()).expect("the scratch path is UTF-8");
        let (source, link) = (root.join("workspace"), root.join("link"));

        fs::create_dir_all(source.join("inside").as_std_path()).expect("the workspace is creatable");
        std::os::unix::fs::symlink(source.join("inside").as_std_path(), link.as_std_path()).expect("the link is creatable");

        // Named outside the workspace, and lexically it is: only resolving it says otherwise.
        let base = gamma_base(&source, Some(&link));

        assert!(!base.starts_with(&source), "{base}");

        let failure = ensure_copy_terminates(&source, &base).expect_err("a base linked into the workspace must be refused");

        assert!(failure.is_usage(), "{failure}");
        assert!(failure.to_string().contains("--cache-dir"), "{failure}");
        assert!(failure.to_string().contains("CARGO_TARGET_DIR"), "{failure}");
        assert!(failure.to_string().contains("build.target-dir"), "{failure}");
    }

    /// The mirror image: a base named *inside* the workspace, so it looks pruned, but reached
    /// through an in-workspace link so the walk never produces the path the exclusion compares
    /// against. Textual containment alone would call this safe and the copy would copy itself.
    #[cfg(unix)]
    #[test]
    fn a_scratch_directory_reached_through_an_in_workspace_link_is_refused() {
        let directory = crate::testing::workdir("scratch-through-link");
        let root = Utf8Path::from_path(directory.path()).expect("the scratch path is UTF-8");

        fs::create_dir_all(root.join("workspace").join("inside").as_std_path()).expect("the workspace is creatable");

        // `workdir` hands back a path holding `..` components while `gamma_base` normalises what it
        // is given, so the two are resolved to the same spelling before they are compared.
        let source = physical(&root.join("workspace"));

        std::os::unix::fs::symlink(source.join("inside").as_std_path(), source.join("link").as_std_path()).expect("the link is creatable");

        let base = gamma_base(&source, Some(&source.join("link")));

        // Named inside the workspace, so the copy's exclusion looks like it will skip it.
        assert!(prunes(&source, &base), "source={source} base={base}");

        let failure = ensure_copy_terminates(&source, &base).expect_err("a base the exclusion cannot match must be refused");

        assert!(failure.is_usage(), "{failure}");
        assert!(failure.to_string().contains("--cache-dir"), "{failure}");
    }

    /// The same shape pointing somewhere genuinely outside stays allowed, which is what makes the
    /// case above a measurement of where the link goes rather than of links in general.
    #[test]
    #[cfg(unix)]
    fn a_scratch_directory_linked_to_somewhere_outside_is_still_allowed() {
        let directory = crate::testing::workdir("scratch-linked-out");
        let root = Utf8Path::from_path(directory.path()).expect("the scratch path is UTF-8");
        let (source, elsewhere, link) = (root.join("workspace"), root.join("elsewhere"), root.join("link"));

        fs::create_dir_all(source.as_std_path()).expect("the workspace is creatable");
        fs::create_dir_all(elsewhere.as_std_path()).expect("the target is creatable");
        std::os::unix::fs::symlink(elsewhere.as_std_path(), link.as_std_path()).expect("the link is creatable");

        let base = gamma_base(&source, Some(&link));

        ensure_copy_terminates(&source, &base).expect("a base that really is outside the workspace is fine");
    }

    /// `prepare` has to ask before it creates anything, or the refusal arrives after the run has
    /// already written a lock file and a tree into the user's workspace.
    #[test]
    fn preparing_with_a_scratch_directory_the_copy_cannot_prune_fails_before_copying() {
        let directory = crate::testing::workdir("scratch-inside-");
        let outer = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let source = outer.join("gamma");

        fs::create_dir_all(source.as_std_path()).expect("the workspace");
        fs::write(source.join("Cargo.toml").as_std_path(), "[workspace]\nmembers = []\n").expect("a manifest");

        let config = Config {
            cache_dir: Some(source.clone()),
            ..Config::default()
        };
        let mut events = crate::testing::Recorder::default();
        let failure = Workspace::prepare(&source, &source.join("target"), &config, &mut events).expect_err("the run must be refused");

        assert!(failure.to_string().contains(absolute(&source).as_str()), "{failure}");
        assert!(
            !source.join("workspace").as_std_path().exists(),
            "the refusal came after the copy had already started"
        );
    }

    /// The copy intentionally excludes all VCS directories. A scratch tree under the source still
    /// sees the same ancestor metadata, but a relocated one does not, so it is refused rather
    /// than letting a build script silently observe a different repository.
    #[test]
    fn relocating_a_workspace_that_exposes_vcs_metadata_is_refused_before_copying() {
        let directory = crate::testing::workdir("scratch-vcs-relocation-");
        let source = Utf8PathBuf::from_path_buf(directory.path().join("source")).expect("UTF-8 path");
        let scratch = Utf8PathBuf::from_path_buf(directory.path().join("external")).expect("UTF-8 path");
        let marker = source.join(".git/HEAD");

        fs::create_dir_all(source.join(".git").as_std_path()).expect("VCS metadata");
        fs::write(marker.as_std_path(), "ref: refs/heads/main\n").expect("VCS metadata");
        fs::write(source.join("Cargo.toml").as_std_path(), "[workspace]\nmembers = []\n").expect("manifest");

        let config = Config {
            cache_dir: Some(scratch),
            ..Config::default()
        };
        let failure = Workspace::prepare(&source, &source.join("target"), &config, &mut crate::testing::Recorder::default())
            .expect_err("a relocated tree must not lose VCS metadata");

        assert!(failure.is_usage(), "{failure}");
        assert!(failure.to_string().contains("--cache-dir"), "{failure}");
        assert!(failure.to_string().contains("CARGO_TARGET_DIR"), "{failure}");
        assert!(failure.to_string().contains("build.target-dir"), "{failure}");
        assert_eq!(
            fs::read_to_string(marker.as_std_path()).expect("VCS metadata"),
            "ref: refs/heads/main\n"
        );
    }

    /// `..` and `.` in a scratch path would leave the copy comparing two spellings of one
    /// directory, which never match, so the exclusion silently stops working.
    #[test]
    fn a_scratch_path_that_names_nothing_is_reduced_to_one_spelling() {
        assert_eq!(absolute(Utf8Path::new("/a/./b/../c")), absolute(Utf8Path::new("/a/c")));

        // Nothing precedes a leading `..`, so it is left to mean what the filesystem says.
        assert!(
            absolute(Utf8Path::new("/../a"))
                .components()
                .any(|component| component == Utf8Component::ParentDir)
        );
    }

    #[test]
    fn a_split_scratch_inside_the_source_is_refused_even_when_the_campaign_base_is_pruned() {
        let source = absolute(Utf8Path::new("/workspace"));
        let scratch = source.join(".cache/cargo-gamma/identity");
        let campaign = source.join("target/cargo-gamma/cache/identity");

        let failure = ensure_copy_layout_terminates(&source, &scratch, &campaign).expect_err("the unexcluded destination must be refused");

        assert!(failure.is_usage(), "{failure}");
        assert!(failure.to_string().contains("split-layout copy"), "{failure}");
    }

    #[test]
    fn a_split_scratch_outside_the_source_is_allowed() {
        ensure_copy_layout_terminates(
            Utf8Path::new("/workspace"),
            Utf8Path::new("/cache/cargo-gamma/identity"),
            Utf8Path::new("/workspace/target/cargo-gamma/cache/identity"),
        )
        .expect("an external destination and pruned campaign cache cannot recurse");
    }

    #[test]
    fn linking_a_package_with_no_manifest_is_a_noop() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        fs::create_dir_all(root.join("crate").join("src").as_std_path()).unwrap();
        let work = unsettled_at(root, Utf8PathBuf::from("/scratch/rt"));
        let files = vec![TargetFile {
            package: "pkg".to_owned(),
            source: None,
            path: Utf8PathBuf::from("crate/src/lib.rs"),
            absolute: Utf8PathBuf::from("/source/crate/src/lib.rs"),
        }];

        work.link_runtime("pkg", &files).unwrap();

        // A scanned file might be malformed or synthetic in tests; absent manifests are ignored
        // rather than making runtime linking fail before instrumentation can explain anything.
        assert!(!work.root.join("Cargo.toml").as_std_path().exists());
    }

    #[test]
    fn manifest_lookup_stops_at_the_workspace_root() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        fs::create_dir_all(root.join("crate").join("src").as_std_path()).unwrap();
        let work = unsettled_at(root, Utf8PathBuf::from("/scratch/rt"));
        let files = vec![TargetFile {
            package: "pkg".to_owned(),
            source: None,
            path: Utf8PathBuf::from("crate/src/lib.rs"),
            absolute: Utf8PathBuf::from("/source/crate/src/lib.rs"),
        }];

        // Walking above the copied root would let an unrelated parent manifest claim this package.
        assert_eq!(work.manifest_of("pkg", &files), None);
    }

    /// A package that was never scanned into the file list has nothing to look a manifest up from;
    /// treating that as "found nowhere" rather than panicking on an empty search keeps a caller
    /// naming an unfamiliar package from crashing the run instead of just skipping the link.
    #[test]
    fn a_package_absent_from_the_scanned_files_has_no_manifest() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        fs::create_dir_all(root.join("crate").join("src").as_std_path()).unwrap();
        let work = unsettled_at(root, Utf8PathBuf::from("/scratch/rt"));
        let files = vec![TargetFile {
            package: "pkg".to_owned(),
            source: None,
            path: Utf8PathBuf::from("crate/src/lib.rs"),
            absolute: Utf8PathBuf::from("/source/crate/src/lib.rs"),
        }];

        assert_eq!(work.manifest_of("someone-else", &files), None);
        work.link_runtime("someone-else", &files)
            .expect("an unknown package is a noop, not an error");
    }

    /// Linking a package whose manifest really is in the copied tree has to write the dependency
    /// rather than merely locate the file: a run that reached instrumentation with the runtime
    /// unlinked would fail every guard's compile with an unresolved path, indistinguishable from a
    /// real bug in the mutated code.
    #[test]
    fn linking_a_package_whose_manifest_exists_adds_the_runtime_dependency() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        let runtime = Utf8PathBuf::from_path_buf(temporary.path().join("rt")).unwrap();

        fs::create_dir_all(root.join("crate").join("src").as_std_path()).unwrap();
        fs::write(
            root.join("crate/Cargo.toml").as_std_path(),
            "[package]\nname = \"pkg\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        let work = unsettled_at(root, runtime);
        let files = vec![TargetFile {
            package: "pkg".to_owned(),
            source: None,
            path: Utf8PathBuf::from("crate/src/lib.rs"),
            absolute: Utf8PathBuf::from("/source/crate/src/lib.rs"),
        }];

        work.link_runtime("pkg", &files).expect("a real manifest must be linkable");

        let manifest = fs::read_to_string(work.root.join("crate/Cargo.toml").as_std_path()).unwrap();

        assert!(manifest.contains(RUNTIME_CRATE), "{manifest}");
    }

    #[test]
    fn overwriting_a_directory_is_refused() {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("not-a-file")).unwrap();
        let root = path.parent().expect("the temporary directory is the root").to_owned();

        fs::create_dir_all(path.as_std_path()).unwrap();

        let cause = Workspace::overwrite(&root, &path, "new").unwrap_err();

        // Instrumentation only writes files the copy already chose; refusing other entry kinds
        // prevents following a link or clobbering a device outside the scratch tree.
        assert!(cause.to_string().contains("refusing to write"), "{cause}");
    }

    /// A path the copy never created has no metadata to inspect at all, and that is reported as
    /// its own failure rather than folded into "refusing to write": the two causes point a reader
    /// in different directions, one at a copy that is missing a file and the other at a link or
    /// device standing where a file should be.
    #[test]
    fn overwriting_a_path_the_copy_never_created_is_reported_rather_than_silently_writing_a_new_file() {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("never-copied.rs")).unwrap();
        let root = path.parent().expect("the temporary directory is the root").to_owned();

        let cause = Workspace::overwrite(&root, &path, "new").unwrap_err();

        assert!(cause.to_string().contains("which the copy did not create"), "{cause}");
    }

    /// Instrumentation exists to replace a file's contents, so both directions matter: a changed
    /// file has to be rewritten and reported as such, and a file already holding what would be
    /// written must be left untouched and reported as a no-op, or the rollback loop would rebuild
    /// every crate every round regardless of what actually changed.
    #[test]
    fn overwriting_a_copied_file_writes_only_when_the_content_actually_differs() {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("lib.rs")).unwrap();
        let root = path.parent().expect("the temporary directory is the root").to_owned();

        fs::write(path.as_std_path(), "old").unwrap();

        let changed = Workspace::overwrite(&root, &path, "new").expect("a real file must be writable");
        assert!(changed, "different content should be reported as written");
        assert_eq!(fs::read_to_string(path.as_std_path()).unwrap(), "new");

        let unchanged = Workspace::overwrite(&root, &path, "new").expect("identical content must still succeed");
        assert!(!unchanged, "identical content should be reported as a no-op");
    }

    /// A file the copy created but that has since become unwritable cannot be instrumented, and
    /// that has to surface as the write failure it is rather than an overwrite that silently kept
    /// stale, uninstrumented source in the tree.
    #[cfg(unix)]
    #[test]
    fn overwriting_a_read_only_file_reports_the_write_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(temporary.path().join("locked.rs")).unwrap();
        let root = path.parent().expect("the temporary directory is the root").to_owned();

        fs::write(path.as_std_path(), "old").unwrap();
        fs::set_permissions(path.as_std_path(), fs::Permissions::from_mode(0o400)).unwrap();

        let cause = Workspace::overwrite(&root, &path, "new").unwrap_err();

        // Restore write permission so the temporary directory can clean itself up.
        fs::set_permissions(path.as_std_path(), fs::Permissions::from_mode(0o600)).unwrap();

        assert!(cause.to_string().contains("could not write"), "{cause}");
    }

    /// `symlink_metadata` is non-following for the last component alone, so a file whose *prefix*
    /// is a link passes that check while the write itself lands wherever the link points. The copy
    /// recreates links verbatim — including absolute targets — so a scratch tree really can hold
    /// one aimed at the user's source. Containment is therefore decided on the physical path.
    #[cfg(unix)]
    #[test]
    fn overwriting_through_a_symlinked_directory_prefix_is_refused() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        let outside = Utf8PathBuf::from_path_buf(temporary.path().join("real")).unwrap();

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(outside.as_std_path()).unwrap();
        fs::write(outside.join("lib.rs").as_std_path(), "user source").unwrap();
        std::os::unix::fs::symlink(outside.as_std_path(), root.join("link").as_std_path()).unwrap();

        let through_the_link = root.join("link").join("lib.rs");

        // The final component is an ordinary file, so the existing guard is satisfied.
        assert!(fs::symlink_metadata(through_the_link.as_std_path()).unwrap().is_file());

        let cause = Workspace::overwrite(&root, &through_the_link, "instrumented")
            .expect_err("a write resolving outside the scratch tree must be refused");

        assert!(cause.to_string().contains("outside the scratch tree"), "{cause}");
        assert_eq!(fs::read_to_string(outside.join("lib.rs").as_std_path()).unwrap(), "user source");
    }

    /// The same escape by the other route: `is_file` resolves every component, so the upward walk
    /// finds the user's real manifest and hands it to `Manifest::save`, which rewrites it.
    #[cfg(unix)]
    #[test]
    fn a_manifest_reached_through_a_symlinked_directory_prefix_is_not_returned() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        let outside = Utf8PathBuf::from_path_buf(temporary.path().join("real")).unwrap();

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::create_dir_all(outside.join("src").as_std_path()).unwrap();
        fs::write(outside.join("Cargo.toml").as_std_path(), "[package]\nname = \"pkg\"\n").unwrap();
        fs::write(outside.join("src").join("lib.rs").as_std_path(), "").unwrap();
        std::os::unix::fs::symlink(outside.as_std_path(), root.join("link").as_std_path()).unwrap();

        let work = Workspace::adopt(root.clone(), root.join("target"));
        let files = vec![TargetFile {
            path: Utf8PathBuf::from("link/src/lib.rs"),
            absolute: root.join("link/src/lib.rs"),
            package: "pkg".to_owned(),
            source: None,
        }];

        assert_eq!(work.manifest_of("pkg", &files), None);
    }

    #[test]
    fn a_second_claim_on_the_same_scratch_directory_is_a_usage_error() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().join("gamma")).unwrap();

        fs::create_dir_all(base.as_std_path()).unwrap();
        let _held = claim(&base).unwrap();
        let cause = claim(&base).unwrap_err();

        // The lock serializes commands targeting the same original workspace.
        assert!(cause.is_usage());
        assert!(cause.to_string().contains("already using"), "{cause}");
    }

    #[test]
    fn claiming_a_lock_never_truncates_its_existing_contents() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_path_buf()).unwrap();
        let path = base.join("lock");
        fs::write(&path, "owner marker").unwrap();

        let held = claim(&base).expect("lock can be claimed");
        drop(held);

        assert_eq!(fs::read_to_string(path).unwrap(), "owner marker");
    }

    /// A filesystem that cannot lock at all is reported as such, not as another run holding it.
    ///
    /// The two call for opposite responses: waiting out a run that does not exist never ends, and
    /// the advice to move the scratch directory cannot help when the new location is on the same
    /// mount. NFS without `lockd` and some CIFS mounts behave this way.
    #[test]
    fn a_filesystem_that_cannot_lock_is_not_reported_as_another_run() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().to_path_buf()).unwrap();
        let _armed = faults::arm(Fault::Lock);

        let cause = claim(&base).unwrap_err();
        let message = cause.to_string();

        assert!(message.contains("could not be taken"), "{message}");
        assert!(!message.contains("already using"), "{message}");
        assert!(!cause.is_usage(), "a filesystem the tool cannot lock is not the user's mistake");
    }

    /// A base directory that does not exist yet cannot have its lock file opened, and that has to
    /// be reported by name rather than treated the same as another run already holding the lock:
    /// the two causes call for opposite fixes, one for a caller that skipped creating the scratch
    /// directory and the other for a run genuinely already in progress.
    #[test]
    fn claiming_a_scratch_directory_that_does_not_exist_reports_the_lock_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let base = Utf8PathBuf::from_path_buf(temporary.path().join("never-created")).unwrap();

        let cause = claim(&base).unwrap_err();

        assert!(cause.to_string().contains("could not open the scratch lock"), "{cause}");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_manifests_are_skipped_while_anchoring() {
        use std::ffi::OsString;

        let temporary = tempfile::tempdir().unwrap();
        let source = Utf8PathBuf::from_path_buf(temporary.path().join("source")).unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        let name = OsString::from_vec(b"bad-\xff".to_vec());
        let bad = root.as_std_path().join(name);

        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("Cargo.toml"), "[package]\nname = \"bad\"\nversion = \"0.0.0\"\n").unwrap();

        anchor_manifests(&source, &root, &root.join("rt")).unwrap();

        // A path that cannot appear in cargo's UTF-8 JSON is left alone rather than poisoning the
        // whole copied tree repair pass.
        assert!(bad.join("Cargo.toml").exists());
    }

    /// A walk over the copied tree passes every file it finds, and only `Cargo.toml` is a manifest;
    /// anything else — a `Cargo.lock`, a source file, a stray artifact — has to be skipped without
    /// being misread as one, or the copy pass would fail on the first ordinary file it walked over.
    #[test]
    fn files_that_are_not_manifests_are_left_alone_while_anchoring() {
        let temporary = tempfile::tempdir().unwrap();
        let source = Utf8PathBuf::from_path_buf(temporary.path().join("source")).unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::write(root.join("Cargo.toml").as_std_path(), "[workspace]\nmembers = []\n").unwrap();
        fs::write(root.join("Cargo.lock").as_std_path(), "# not a manifest\n").unwrap();
        fs::write(root.join("lib.rs").as_std_path(), "pub fn f() {}\n").unwrap();

        anchor_manifests(&source, &root, &root.join("rt")).unwrap();

        assert_eq!(
            fs::read_to_string(root.join("Cargo.lock").as_std_path()).unwrap(),
            "# not a manifest\n"
        );
        assert_eq!(fs::read_to_string(root.join("lib.rs").as_std_path()).unwrap(), "pub fn f() {}\n");
    }

    #[test]
    fn an_external_manifest_link_is_refused_before_its_target_is_rewritten() {
        let temporary = tempfile::tempdir().unwrap();
        let source = Utf8PathBuf::from_path_buf(temporary.path().join("source")).unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        let outside = Utf8PathBuf::from_path_buf(temporary.path().join("outside.toml")).unwrap();
        let original = "[package]\nname = \"outside\"\nversion = \"0.0.0\"\n\n[dependencies]\nshared = { path = \"../shared\" }\n";

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::write(outside.as_std_path(), original).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.as_std_path(), root.join("Cargo.toml").as_std_path()).unwrap();
        #[cfg(windows)]
        if let Err(cause) = std::os::windows::fs::symlink_file(outside.as_std_path(), root.join("Cargo.toml").as_std_path()) {
            assert_eq!(cause.raw_os_error(), Some(1314), "unexpected symlink creation failure: {cause}");
            eprintln!("Windows file-symlink fixture needs Developer Mode or symbolic-link privilege: {cause}");
            return;
        }

        let failure = anchor_manifests(&source, &root, &root.join("rt")).expect_err("an external manifest must not be rewritten");

        assert!(failure.to_string().contains("outside"), "{failure}");
        assert!(failure.message().starts_with("refusing to use a scratch manifest "), "{failure}");
        assert_eq!(fs::read_to_string(outside.as_std_path()).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn an_external_cargo_configuration_link_is_refused_before_its_target_is_rewritten() {
        let temporary = tempfile::tempdir().unwrap();
        let source = Utf8PathBuf::from_path_buf(temporary.path().join("source")).unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        let outside = Utf8PathBuf::from_path_buf(temporary.path().join("outside-config.toml")).unwrap();
        let original = "paths = [\"../shared\"]\n";

        fs::create_dir_all(root.join(".cargo").as_std_path()).unwrap();
        fs::write(outside.as_std_path(), original).unwrap();
        std::os::unix::fs::symlink(outside.as_std_path(), root.join(".cargo/config.toml").as_std_path()).unwrap();

        let failure = anchor_manifests(&source, &root, &root.join("rt")).expect_err("an external config must not be rewritten");

        assert!(failure.to_string().contains("outside"), "{failure}");
        assert_eq!(fs::read_to_string(outside.as_std_path()).unwrap(), original);
    }
}

#[cfg(any(test, feature = "internals"))]
pub(super) mod native_test_util;
#[cfg(any(test, feature = "internals"))]
#[doc(hidden)]
pub(crate) use native_test_util::{create_private_dir_all_for_test, mark_cache_owned_for_test};
