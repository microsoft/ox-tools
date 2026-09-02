// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::error::Error as StdError;
use core::num::NonZeroUsize;
#[cfg(any(test, feature = "internals"))]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicBool, Ordering};
use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::io::{Read as _, Write as _};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::process::{Command, Output};
use std::sync::OnceLock;
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
use super::manifest::{CAP_LINTS, Manifest, RUNTIME_CRATE, RUNTIME_PACKAGE, anchor_cargo_config, cap_lints};
use super::nextest::Harness;
use super::sync::sync_or_copy;
use super::test_binary::{TEST_THREADS_VAR, TestBinary, harness_threads};
use crate::Result;
use crate::discover::TargetFile;
use crate::error::error;

/// The workspace and optional redirected-cache locks claimed as one handoff unit.
pub(crate) struct CacheLocks {
    workspace: File,
    redirected: Option<File>,
    #[cfg(any(test, feature = "internals"))]
    identity: usize,
}

#[cfg(any(test, feature = "internals"))]
static NEXT_CACHE_LOCK_IDENTITY: AtomicUsize = AtomicUsize::new(1);

/// Identifies one claimed lock pair across the adoption-to-preparation handoff.
#[cfg(any(test, feature = "internals"))]
pub(crate) const fn cache_lock_identity(locks: &CacheLocks) -> usize {
    locks.identity
}

/// The guard runtime's sources, embedded so that the vendored copy cannot drift from the real one.
const RUNTIME_SOURCES: [(&str, &str); 3] = [
    ("lib.rs", include_str!("../../../cargo-gamma-rt/src/lib.rs")),
    ("either.rs", include_str!("../../../cargo-gamma-rt/src/either.rs")),
    ("runtime.rs", include_str!("../../../cargo-gamma-rt/src/runtime.rs")),
];

/// The workspace package contract inherited by the real runtime crate.
const WORKSPACE_MANIFEST: &str = include_str!("../../../../Cargo.toml");

/// Identifies the workspace allowed to reuse a cache directory.
const CACHE_OWNER: &str = ".cargo-gamma-owner";

/// Generous marker bound allowing 32,767 Windows UTF-16 code units at four UTF-8 bytes each.
const MAX_CACHE_OWNER_LEN: u64 = 32_767 * 4;

/// A scratch copy of the workspace, instrumented and ready to build.
#[derive(Debug)]
pub struct Workspace {
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

    /// Held for the life of the run so that another cargo-gamma command targeting the same original
    /// workspace is turned away. Released when the process ends, however it ends, so a crash cannot
    /// leave a lock nobody can clear.
    _workspace_lock: File,

    /// Held when reusable state was redirected, so another workspace cannot use the same cache.
    _cache_lock: Option<File>,

    /// Whether [`Workspace::teardown`] has already removed what the destructor would remove.
    ///
    /// The destructor is the fallback for the paths that never reach an explicit teardown — an
    /// error part-way through a run, a panic, a test that drops the tree without ceremony — so it
    /// has to stay. This is what keeps it from walking a tree that is already gone.
    torn_down: bool,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if self.torn_down {
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
    pub(super) fn prepare(source: &Utf8Path, config: &Config, events: &mut impl Events) -> Result<Self> {
        Self::prepare_with_locks(source, config, events, None)
    }

    /// Copies and instruments the tree while retaining locks taken before cache adoption.
    pub(super) fn prepare_with_locks(
        source: &Utf8Path,
        config: &Config,
        events: &mut impl Events,
        locks: Option<CacheLocks>,
    ) -> Result<Self> {
        config.cargo.validate()?;

        // Absolute from here on, so that the copy's own exclusion — which compares the scratch base
        // against the paths it walks — is comparing two paths of the same kind.
        let source = &absolute(source);
        let base = gamma_base(source, config.cache_dir.as_deref());

        ensure_copy_terminates(source, &base)?;

        let root = base.join("workspace");
        if config.cache_dir.is_some() {
            ensure_vcs_visibility(source, &root)?;
        }
        let target = base.join("target");
        let runtime = base.join("rt");

        events.begin("Copying", "Copied", "the workspace");

        fs::create_dir_all(base.as_std_path())
            .map_err(|cause| error!("could not create the scratch directory at `{base}`").caused_by(cause))?;

        let locks = match locks {
            Some(locks) => locks,
            None => claim_cache(source, config.cache_dir.as_deref())?,
        };
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

        let workspace = Self {
            root,
            target,
            libraries,
            cargo: config.cargo.clone(),
            runtime,
            leak: config.leak_dirs,
            settled: AtomicBool::new(false),
            nextest: None,
            launch: OnceLock::new(),
            harness_threads: OnceLock::new(),
            _workspace_lock: workspace_lock,
            _cache_lock: cache_lock,
            torn_down: false,
        };

        events.end("");

        Ok(workspace)
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

    /// Runs a cargo command in the copied tree.
    /// Wraps an existing directory as a workspace, without copying or locking anything.
    ///
    /// Only for tests that need a real tree to run cargo in. The tree is left in place on drop,
    /// because it belongs to whatever created it rather than to this handle.
    #[cfg(any(test, feature = "internals"))]
    pub(crate) fn adopt(root: Utf8PathBuf, target: Utf8PathBuf) -> Self {
        Self {
            runtime: root.join("gamma-rt"),
            root,
            target,
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            nextest: None,
            settled: AtomicBool::new(true),
            leak: true,
            launch: OnceLock::new(),
            harness_threads: OnceLock::new(),
            _workspace_lock: tempfile::tempfile().expect("a temporary file should be creatable"),
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

    pub(super) fn cargo(&self) -> Command {
        let mut command = Command::new(cargo_binary());

        let _ = command.current_dir(self.root.as_std_path());
        let _ = command.env("CARGO_TARGET_DIR", self.target.as_std_path());
        let _ = command.env("CARGO_TERM_COLOR", if self.cargo.color { "always" } else { "never" });

        cap_ambient_rustflags(&mut command);

        // A guard is inert unless this names its mutant. Proc macros run inside the compiler, so a
        // live mutant in one executes during the build rather than during a test — an infinite loop
        // there hangs the one build the whole run depends on, with no test to time it out. The
        // variable is scrubbed rather than trusted to be absent, since test processes set it and a
        // user debugging a mutant by hand may export it.
        let _ = command.env_remove(gamma_rt::ACTIVE_VAR);

        command
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
    pub(super) fn root(&self) -> &Utf8Path {
        &self.root
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

    /// Settles how wide each spawned test harness runs, from the run's own job count.
    ///
    /// Called once, before anything is spawned, so that the baseline and every mutant after it are
    /// measured under the same contention. See [`Workspace::harness_threads`].
    pub(super) fn calibrate_harness(&self, jobs: usize) {
        let cores = thread::available_parallelism().map_or(1, NonZeroUsize::get);
        let inherited = env::var(TEST_THREADS_VAR).ok();

        let _settled = self
            .harness_threads
            .set(harness_threads(jobs, cores, inherited.as_deref()).map(|threads| threads.to_string()));
    }

    /// How many threads to tell a spawned test harness to use, or `None` to leave it alone.
    pub(super) fn harness_threads(&self) -> Option<&str> {
        self.harness_threads.get().and_then(Option::as_deref)
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
        self.nextest = Some(Harness::prepare(self, binaries)?);

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
        interpret(
            contained_output(command, MemoryRequest::default()).map(Captured::from),
            what,
            &self.root,
        )
    }

    /// The gamma base this run's tree sits under, whose size is the run's [`footprint`].
    pub(super) fn base(&self) -> &Utf8Path {
        self.root.parent().unwrap_or(&self.root)
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

/// Where this tool keeps everything it generates for a workspace.
///
/// Outside the workspace by default, so Cargo cannot rediscover and merge the real workspace's
/// ancestor configuration after the copied configuration has already been rewritten.
///
/// The answer is always absolute. A relative `--cache-dir` is resolved against the current
/// directory, which is what a user typing one means, and absolutising here — before the tree, the
/// build directory and the vendored runtime are derived from it — is what makes every one of those
/// absolute by construction. It has to be: the copy prunes its own destination by comparing it
/// against the absolute paths it walks, and cargo resolves a path dependency against the manifest
/// holding it, so a relative base would make the copy descend into its own output and the
/// instrumented tree point its runtime dependency at itself.
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

/// The directory every default per-workspace cache is a child of.
fn default_cache_home(root: &Utf8Path) -> Utf8PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .and_then(absolute_cache_home)
        .or_else(|| env::var_os("LOCALAPPDATA").and_then(absolute_cache_home))
        .or_else(|| env::var_os("HOME").and_then(absolute_cache_home).map(|home| home.join(".cache")))
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
fn workspace_identity(root: &Utf8Path) -> String {
    let root = physical(root);
    digest_workspace_path(&root)
}

/// Digests the already-normalized spelling that defines one workspace's cache identity.
fn digest_workspace_path(root: &Utf8Path) -> String {
    let digest = blake3::hash(root.as_str().as_bytes());
    let mut identity = [0_u8; 8];

    identity.copy_from_slice(digest.as_bytes().get(..8).expect("a BLAKE3 digest is 32 bytes long"));

    format!("{:016x}", u64::from_be_bytes(identity))
}

/// Deletes every cached entry for a workspace while preserving its concurrency lock and identity.
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
pub fn clean_cache(root: &Utf8Path) -> Result<bool> {
    let base = gamma_base(root, None);
    let mut removed = false;

    if base.exists() {
        reject_linked_cache(&base)?;
        let _lock = claim(&base)?;
        validate_cache_owner(root, &base, CacheKind::Default)?;
        let entries =
            fs::read_dir(base.as_std_path()).map_err(|cause| error!("could not read cargo-gamma's cache at `{base}`").caused_by(cause))?;
        for entry in entries {
            let entry = entry.map_err(|cause| error!("could not read an entry in cargo-gamma's cache at `{base}`").caused_by(cause))?;

            if entry.file_name() == "lock" || entry.file_name() == CACHE_OWNER {
                continue;
            }

            remove_cached(&entry)?;
            removed = true;
        }
    }

    Ok(removed)
}

/// Removes one entry of a cache directory, whatever kind of thing it is.
///
/// A symbolic link is unlinked rather than followed, so a link planted in a cache cannot turn a
/// clean into a recursive delete of whatever it points at.
fn remove_cached(entry: &fs::DirEntry) -> Result<()> {
    let path = entry.path();
    let file_type = entry
        .file_type()
        .map_err(|cause| error!("could not inspect cached data at `{}`", path.display()).caused_by(cause))?;
    let result = if file_type.is_dir() && !file_type.is_symlink() {
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    };

    result.map_err(|cause| error!("could not remove cached data at `{}`", path.display()).caused_by(cause))
}

/// Resolves a path against the current directory and removes the components that name nothing.
///
/// Purely textual, so it never touches the filesystem and never fails: the paths it is given are
/// scratch directories that do not exist yet, which is exactly when canonicalising cannot answer.
pub(super) fn absolute(path: &Utf8Path) -> Utf8PathBuf {
    let rooted = if path.is_absolute() {
        path.to_owned()
    } else {
        // A current directory that is not UTF-8, or that has been removed out from under the
        // process, leaves the path as written rather than failing a run over it.
        env::current_dir()
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
             Point --cache-dir at a directory that is really outside the workspace."
        )
        .usage());
    }

    Err(error!(
        "the scratch directory `{base}` is the workspace at `{source}` itself, so copying the \
         workspace would copy the copy.\n\
         Point --cache-dir at a directory outside the workspace."
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

    if source_metadata.is_empty() {
        return Ok(());
    }

    let scratch_metadata = visible_vcs_metadata(scratch);
    let hidden: Vec<&Utf8PathBuf> = source_metadata.iter().filter(|marker| !scratch_metadata.contains(marker)).collect();

    if hidden.is_empty() {
        return Ok(());
    }

    Err(error!(
        "`--cache-dir` would relocate the cached workspace to `{scratch}`, where build scripts cannot see VCS metadata available from `{source}`: {}. \
         Use a cache directory beneath the same repository, or remove the build-time VCS dependency.",
        hidden.iter().map(|marker| marker.as_str()).collect::<Vec<_>>().join(", ")
    )
    .usage())
}

/// Makes source-control metadata visible from an isolated default scratch tree.
fn expose_vcs_metadata(source: &Utf8Path, scratch: &Utf8Path) -> Result<()> {
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

                absolute(&marker.parent().unwrap_or_else(|| Utf8Path::new("")).join(relative))
            };

            fs::write(destination.as_std_path(), format!("gitdir: {git_dir}\n"))
                .map_err(|cause| error!("could not expose Git metadata at `{destination}`").caused_by(cause))?;

            continue;
        }

        #[cfg(unix)]
        std::os::unix::fs::symlink(marker.as_std_path(), destination.as_std_path())
            .map_err(|cause| error!("could not expose VCS metadata at `{destination}`").caused_by(cause))?;

        #[cfg(windows)]
        {
            let linked = if marker.as_std_path().is_dir() {
                std::os::windows::fs::symlink_dir(marker.as_std_path(), destination.as_std_path())
            } else {
                std::os::windows::fs::symlink_file(marker.as_std_path(), destination.as_std_path())
            };

            linked.map_err(|cause| error!("could not expose VCS metadata at `{destination}`").caused_by(cause))?;
        }
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
    if base.exists() {
        reject_linked_cache(base)?;

        match fs::symlink_metadata(base.join(CACHE_OWNER).as_std_path()) {
            Ok(_metadata) => {}
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => {
                if has_any_entries(base)? {
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
/// A redirected cache is named by the user and everything under it is later handed to cargo as
/// `CARGO_TARGET_DIR` and executed from. A symbolic link — or, on Windows, any other reparse point
/// — means the name that was checked and the directory that is used are two different things, and
/// whoever can rewrite the link chooses the second one after the first has been approved. Only the
/// final component is checked here; the ancestry above it is covered by
/// [`reject_foreign_writers`] on the platforms where ownership can be established at all.
fn reject_linked_cache(base: &Utf8Path) -> Result<()> {
    let metadata = fs::symlink_metadata(base.as_std_path())
        .map_err(|cause| error!("could not inspect the redirected cargo-gamma cache at `{base}`").caused_by(cause))?;

    if metadata.file_type().is_symlink() {
        return Err(error!(
            "the redirected cache at `{base}` is a link.\n\
             Pass the directory itself to --cache-dir: a link can be repointed after it has been checked, \
             at a directory whose contents this tool would then build and run."
        )
        .usage());
    }

    if !metadata.is_dir() {
        return Err(error!(
            "the redirected cache at `{base}` is not a directory.\n\
             Choose an empty directory for --cache-dir."
        )
        .usage());
    }

    Ok(())
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
    let owner = base.join(CACHE_OWNER);
    let source = physical(source);

    match open_cache_owner(&owner) {
        Ok(mut marker) => {
            let metadata = marker
                .metadata()
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

            if u64::try_from(recorded.len()).unwrap_or(u64::MAX) != metadata.len() {
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
            if has_unowned_entries(base)? {
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

            marker
                .write_all(source.as_str().as_bytes())
                .map_err(|cause| error!("could not write the cargo-gamma cache owner marker at `{owner}`").caused_by(cause))?;
        }
        Err(cause) => {
            return Err(error!("could not inspect the cargo-gamma cache owner marker at `{owner}`").caused_by(cause));
        }
    }

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

    fs::create_dir_all(base.as_std_path())
        .map_err(|cause| error!("could not create cargo-gamma's workspace cache at `{base}`").caused_by(cause))?;

    let lock = claim(&base)?;

    validate_cache_owner(root, &base, CacheKind::Default)?;

    Ok(lock)
}

/// Takes every lock needed to use one workspace's reusable state.
pub(crate) fn claim_cache(root: &Utf8Path, cache: Option<&Utf8Path>) -> Result<CacheLocks> {
    let workspace = claim_workspace(root)?;
    let base = gamma_base(root, cache);
    let default = gamma_base(root, None);
    let redirected = if cache.is_some() && physical(&base) != physical(&default) {
        Some(claim_redirected_cache(root, &base)?)
    } else {
        None
    };

    Ok(CacheLocks {
        workspace,
        redirected,
        #[cfg(any(test, feature = "internals"))]
        identity: NEXT_CACHE_LOCK_IDENTITY.fetch_add(1, Ordering::Relaxed),
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
    for entry in WalkDir::new(root.as_std_path()).into_iter().filter_map(core::result::Result::ok) {
        if entry.file_name() != "Cargo.toml" {
            continue;
        }

        let Some(path) = Utf8Path::from_path(entry.path()) else {
            continue;
        };

        let Some(original) = path.parent().and_then(|directory| directory.strip_prefix(root).ok()) else {
            continue;
        };

        let _destination = crate::paths::require_within(path, root, "a scratch manifest")?;
        let mut manifest = Manifest::read(path)?;

        manifest.anchor_paths(&source.join(original), original);
        manifest.redirect_runtime(runtime)?;
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
    // The encoded spelling separates flags with a unit separator rather than a space, which is the
    // whole reason it exists: a flag may contain spaces.
    if let Some(inherited) = env::var_os("CARGO_ENCODED_RUSTFLAGS") {
        return extend(command, "CARGO_ENCODED_RUSTFLAGS", inherited, "\u{1f}");
    }

    if let Some(inherited) = env::var_os("RUSTFLAGS") {
        return extend(command, "RUSTFLAGS", inherited, " ");
    }

    for (name, inherited) in env::vars_os() {
        let Some(name) = name.to_str() else {
            continue;
        };

        if name.starts_with("CARGO_TARGET_") && name.ends_with("_RUSTFLAGS") {
            extend(command, name, inherited, " ");
        }
    }

    if let Some(inherited) = env::var_os("CARGO_BUILD_RUSTFLAGS") {
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

    let workspace: toml::Value = toml::from_str(WORKSPACE_MANIFEST)
        .map_err(|cause| error!("could not read the embedded workspace package contract").caused_by(cause))?;
    let package = workspace
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .and_then(toml::Value::as_table)
        .ok_or_else(|| error!("the embedded workspace manifest has no `[workspace.package]` table"))?;
    let inherited = |name| {
        package
            .get(name)
            .and_then(toml::Value::as_str)
            .ok_or_else(|| error!("the embedded workspace package contract has no string `{name}`"))
    };
    let edition = inherited("edition")?;
    let rust_version = inherited("rust-version")?;
    let manifest = format!(
        "[package]\nname = \"{RUNTIME_PACKAGE}\"\nversion = \"0.0.0\"\nedition = \"{edition}\"\nrust-version = \"{rust_version}\"\npublish = false\n\n\
         [features]\nloom = []\n\n[lints.rust]\nunexpected_cfgs = {{ level = \"warn\", check-cfg = ['cfg(coverage_nightly)', 'cfg(loom)'] }}\n\n\
         [lib]\nname = \"{RUNTIME_CRATE}\"\npath = \"src/lib.rs\"\n\n[workspace]\n"
    );

    fs::write(at.join("Cargo.toml").as_std_path(), manifest)
        .map_err(|cause| error!("could not write the runtime manifest in `{at}`").caused_by(cause))?;

    for (name, contents) in RUNTIME_SOURCES {
        let path = source.join(name);

        fs::write(path.as_std_path(), contents).map_err(|cause| error!("could not write the runtime source `{path}`").caused_by(cause))?;
    }

    Ok(())
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    #[cfg(windows)]
    use std::os::windows::fs::symlink_dir;

    use super::*;

    /// Redirected-cache fixtures must not inherit permissions from the repository checkout:
    /// containerized build agents may mount that checkout under a non-sticky shared directory,
    /// which is exactly an unsafe cache ancestry the production check must reject.
    fn private_system_tempdir(prefix: &str) -> tempfile::TempDir {
        let mut builder = tempfile::Builder::new();

        builder.prefix(prefix);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            builder.permissions(fs::Permissions::from_mode(0o700));
        }

        builder
            .tempdir()
            .expect("the redirected-cache fixture should be creatable in the system temporary directory")
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

    /// Exercises every step of preparing a scratch tree — creating the base, claiming the lock,
    /// copying, vendoring the runtime, and anchoring manifests.
    #[test]
    fn preparing_a_fresh_workspace_produces_a_usable_scratch_tree() {
        let directory = crate::testing::workdir("prepare-happy-");
        let source = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");

        fs::write(
            source.join("Cargo.toml").as_std_path(),
            "[package]\nname = \"trivial\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .expect("a manifest");
        fs::create_dir_all(source.join("src").as_std_path()).expect("src");
        fs::write(source.join("src/lib.rs").as_std_path(), "pub const A: i32 = 1;\n").expect("lib");

        let config = Config::default();
        let mut events = crate::testing::Recorder::default();
        let work = Workspace::prepare(&source, &config, &mut events).expect("a fresh tree must prepare cleanly");

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

        let CacheLocks {
            workspace: _workspace,
            redirected,
            #[cfg(any(test, feature = "internals"))]
                identity: _identity,
        } = claim_cache(&source, Some(&base)).expect("the default cache has one lock domain");

        assert!(redirected.is_none());
    }

    #[test]
    fn cleaning_removes_cache_data_but_not_published_reports() {
        let directory = crate::testing::workdir("clean-cache-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the workspace path is UTF-8");
        let base = gamma_base(&root, None);
        let report = root.join("target/cargo-gamma/gamma-report.json");

        fs::create_dir_all(&base).expect("cache");
        validate_cache_owner(&root, &base, CacheKind::Default).expect("claim cache");
        fs::create_dir(base.join("workspace")).expect("cached workspace");
        fs::create_dir_all(base.join("target")).expect("cached target");
        fs::write(base.join("last-gamma-run.json"), "{}").expect("run record");
        fs::create_dir_all(report.parent().expect("report directory")).expect("report directory");
        fs::write(&report, "{}").expect("published report");

        assert!(clean_cache(&root).expect("clean cache"));
        assert!(!base.join("workspace").exists());
        assert!(!base.join("target").exists());
        assert!(!base.join("last-gamma-run.json").exists());
        assert!(base.join("lock").exists(), "the concurrency lock remains");
        assert!(report.exists(), "published output is not cache data");
        assert!(!clean_cache(&root).expect("cleaning an empty cache"));
    }

    #[test]
    fn cleaning_refuses_an_unmarked_populated_cache() {
        let directory = crate::testing::workdir("clean-unmarked-cache-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the workspace path is UTF-8");
        let base = gamma_base(&root, None);

        fs::create_dir_all(base.join("workspace")).expect("unowned contents");

        let failure = clean_cache(&root).expect_err("unowned contents must not be removed");

        assert!(failure.is_usage(), "{failure}");
        assert!(base.join("workspace").exists(), "unowned contents must survive");
        assert!(!base.join(CACHE_OWNER).exists(), "the cache must not be adopted");
    }

    #[test]
    fn cleaning_refuses_a_cache_owned_by_an_active_run() {
        let directory = crate::testing::workdir("clean-active-cache-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the workspace path is UTF-8");
        let base = gamma_base(&root, None);

        fs::create_dir_all(&base).expect("cache");
        let _held = claim(&base).expect("active run lock");
        let failure = clean_cache(&root).expect_err("an active cache must not be cleaned");

        assert!(failure.to_string().contains("already using"), "{failure}");
    }

    #[test]
    fn the_vendored_runtime_is_the_real_one() {
        // If these ever diverge, guards would be compiled against a runtime that is not the one
        // this build was tested with.
        let runtime = RUNTIME_SOURCES
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
        for (name, _contents) in RUNTIME_SOURCES {
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
            root: root.to_owned(),
            runtime: base.join("rt"),
            target: target.to_owned(),
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            nextest: None,
            settled: AtomicBool::new(false),
            leak: false,
            launch: OnceLock::new(),
            harness_threads: OnceLock::new(),
            _workspace_lock: File::create(base.join("lock").as_std_path()).unwrap(),
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
    fn env_child_helper() {
        let Ok(scenario) = env::var(CHILD_SCENARIO_VAR) else {
            return;
        };

        let mut payload: Vec<(&str, String)> = Vec::new();

        match scenario.as_str() {
            "cargo_binary" => payload.push(("cargo", cargo_binary())),
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
    fn run_child(scenario: &str, vars: &[(&str, Option<&str>)]) -> BTreeMap<String, String> {
        let executable = env::current_exe().expect("the test binary knows its own path");
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
    fn parse_payload(stdout: &str) -> Option<BTreeMap<String, String>> {
        let mut lines = stdout.lines().skip_while(|line| *line != PAYLOAD_OPEN);

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

    /// [`unsettled_default`] over a real directory, for the tests that touch the tree.
    ///
    /// Spelled out rather than deriving from [`unsettled_default`] with struct update syntax:
    /// `Workspace` implements `Drop`, so its fields cannot be moved out of another instance.
    fn unsettled_at(root: Utf8PathBuf, runtime: Utf8PathBuf) -> Workspace {
        Workspace {
            root,
            runtime,
            target: Utf8PathBuf::from("/scratch/build"),
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            nextest: None,
            settled: AtomicBool::new(true),
            leak: true,
            launch: OnceLock::new(),
            harness_threads: OnceLock::new(),
            _workspace_lock: tempfile::tempfile().unwrap(),
            _cache_lock: None,
            torn_down: false,
        }
    }

    /// A workspace over no real directory, for tests that only inspect the `Command` it builds.
    ///
    /// Leaked rather than torn down: nothing was ever created, so nothing should be removed.
    fn unsettled_default() -> Workspace {
        Workspace {
            root: Utf8PathBuf::from("/tmp/gamma-root"),
            runtime: Utf8PathBuf::from("/tmp/gamma-rt"),
            target: Utf8PathBuf::from("/tmp/gamma-target"),
            libraries: Vec::new(),
            cargo: CargoOptions::default(),
            nextest: None,
            settled: AtomicBool::new(true),
            leak: true,
            launch: OnceLock::new(),
            harness_threads: OnceLock::new(),
            _workspace_lock: tempfile::tempfile().unwrap(),
            _cache_lock: None,
            torn_down: false,
        }
    }

    /// `CARGO_ENCODED_RUSTFLAGS` uses a unit separator rather than a space, and cargo prefers it
    /// over the plain form; extending it in place rather than replacing it keeps whatever flags the
    /// user's own environment configured instead of silently dropping them from the instrumented
    /// build.
    #[test]
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
    fn only_absolute_environment_cache_homes_define_the_lock_domain() {
        let absolute = absolute(Utf8Path::new("."));

        assert_eq!(absolute_cache_home(absolute.clone().into()), Some(absolute));
        assert_eq!(absolute_cache_home(OsString::from("relative/cache")), None);
        assert_eq!(absolute_cache_home(OsString::new()), None);
    }

    /// Filesystem aliases of one Windows workspace share one cache and lock identity.
    #[cfg(windows)]
    #[test]
    fn case_aliases_of_one_workspace_share_the_default_cache_identity() {
        let directory = crate::testing::workdir("workspace-case-identity");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the temporary path is UTF-8");
        let alias = Utf8PathBuf::from(root.as_str().to_uppercase());

        assert_eq!(workspace_identity(&root), workspace_identity(&alias));
        assert_eq!(gamma_base(&root, None), gamma_base(&alias, None));
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
        let base = gamma_base(root, None);

        assert!(!base.starts_with(root), "{base}");
        assert!(
            !base.ancestors().any(|ancestor| ancestor == root.join(".cargo")),
            "the real workspace configuration remains in Cargo's scratch ancestor chain: {base}"
        );
    }

    #[test]
    fn array_rustflags_from_workspace_config_reach_scratch_cargo_once() {
        let directory = crate::testing::workdir("scratch-config-once");
        let source = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf8");
        let capture = source.join("captured-flags");

        fs::create_dir_all(source.join(".cargo").as_std_path()).expect(".cargo");
        fs::create_dir_all(source.join("src").as_std_path()).expect("src");
        fs::write(
            source.join("Cargo.toml").as_std_path(),
            "[package]\nname = \"scratch-config-once\"\nversion = \"0.0.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\n[workspace]\n",
        )
        .expect("manifest");
        fs::write(source.join("src/lib.rs").as_std_path(), "").expect("lib");
        fs::write(
            source.join("build.rs").as_std_path(),
            "fn main() { std::fs::write(std::env::var(\"GAMMA_CAPTURE\").unwrap(), std::env::var(\"CARGO_ENCODED_RUSTFLAGS\").unwrap()).unwrap(); }\n",
        )
        .expect("build script");
        fs::write(
            source.join(".cargo/config.toml").as_std_path(),
            "[build]\nrustflags = [\"--cfg\", \"gamma_once\"]\n",
        )
        .expect("config");

        let mut events = crate::testing::Recorder::default();
        let work = Workspace::prepare(&source, &Config::default(), &mut events).expect("prepare");
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
        let inside = gamma_base(Utf8Path::new("/workspace"), None);

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
        let failure = Workspace::prepare(&source, &config, &mut events).expect_err("the run must be refused");

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
        let failure = Workspace::prepare(&source, &config, &mut crate::testing::Recorder::default())
            .expect_err("a relocated tree must not lose VCS metadata");

        assert!(failure.is_usage(), "{failure}");
        assert!(failure.to_string().contains("--cache-dir"), "{failure}");
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
    fn linking_a_package_with_no_manifest_is_a_noop() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        fs::create_dir_all(root.join("crate").join("src").as_std_path()).unwrap();
        let work = unsettled_at(root, Utf8PathBuf::from("/scratch/rt"));
        let files = vec![TargetFile {
            package: "pkg".to_owned(),
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
        use std::os::unix::ffi::OsStringExt as _;

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

    #[cfg(unix)]
    #[test]
    fn an_external_manifest_link_is_refused_before_its_target_is_rewritten() {
        let temporary = tempfile::tempdir().unwrap();
        let source = Utf8PathBuf::from_path_buf(temporary.path().join("source")).unwrap();
        let root = Utf8PathBuf::from_path_buf(temporary.path().join("tree")).unwrap();
        let outside = Utf8PathBuf::from_path_buf(temporary.path().join("outside.toml")).unwrap();
        let original = "[package]\nname = \"outside\"\nversion = \"0.0.0\"\n\n[dependencies]\nshared = { path = \"../shared\" }\n";

        fs::create_dir_all(root.as_std_path()).unwrap();
        fs::write(outside.as_std_path(), original).unwrap();
        std::os::unix::fs::symlink(outside.as_std_path(), root.join("Cargo.toml").as_std_path()).unwrap();

        let failure = anchor_manifests(&source, &root, &root.join("rt")).expect_err("an external manifest must not be rewritten");

        assert!(failure.to_string().contains("outside"), "{failure}");
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
