// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Copying a source tree into the scratch directory.
//!
//! Nothing here checks for interruption. The copy precedes the run's first contained spawn, so no
//! signal handler is armed yet and Ctrl-C takes effect immediately by the default disposition. The
//! scratch tree is left behind for the next run to clear, and the operating system releases its
//! lock when the process dies.

use core::sync::atomic::{AtomicBool, Ordering};
use std::fs::{self, File, FileTimes};
use std::io::ErrorKind;
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::SystemTime;

use camino::{Utf8Path, Utf8PathBuf};
use ignore::{WalkBuilder, WalkState};

use crate::error::{Error, error};
use crate::{HashMap, Result};

/// Version control directories, which are large, hold nothing a build reads, and are actively
/// hazardous in a tree a tool is rewriting: a stray command run from the scratch copy could commit
/// instrumented source over the user's work.
pub(super) const VCS_DIRS: [&str; 7] = [".git", ".hg", ".bzr", ".svn", "_darcs", ".jj", ".pijul"];

/// The VCS metadata visible from a source or scratch location.
///
/// The copy deliberately omits every [`VCS_DIRS`] entry, so an external scratch location is only
/// semantically equivalent when it can still find the same metadata through one of its ancestors.
pub(super) fn visible_vcs_metadata(path: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut found = Vec::new();
    let mut directory = crate::paths::physical(path).unwrap_or_else(|_unresolved| path.to_path_buf());

    loop {
        for name in VCS_DIRS {
            let marker = directory.join(name);

            if fs::symlink_metadata(marker.as_std_path()).is_ok() {
                found.push(crate::paths::physical(&marker).unwrap_or(marker));
            }
        }

        let Some(parent) = directory.parent() else {
            break;
        };

        if parent == directory {
            break;
        }

        directory = parent.to_path_buf();
    }

    found.sort();
    found.dedup();
    found
}

/// Whether copy-on-write cloning is still worth attempting for one copy destination.
///
/// A filesystem either supports reflinks or does not, so one failure settles it for every later
/// file written to the same destination and the rest of the copy goes straight to a byte-for-byte
/// read. The check is a latch rather than a per-file probe because the failure is not always
/// reported as [`std::io::ErrorKind::Unsupported`] — some platforms return a plain permission or
/// argument error — so there is nothing reliable to match on.
///
/// Scoped to the destination tree rather than to the process, because "can this be cloned" is a
/// property of where the copy is being written and one run writes to more than one place: the
/// default scratch tree lives under the user's cache directory and a `--cache-dir` routinely names
/// another mount. A process-wide latch let the first failure anywhere disable cloning everywhere
/// afterwards, including on destinations that support it. Nothing was ever copied wrongly — the
/// byte-for-byte fallback is exact — but every later copy paid full price for one unrelated mount.
///
/// The destination tree, rather than the filesystem it sits on, is the key: it is what the caller
/// names, a run has only a handful of them, and two trees on one mount that each learn the same
/// answer cost one redundant clone attempt apiece. Deriving a filesystem identity instead would buy
/// that one attempt back and, since every test's scratch tree is on the same mount as every other
/// test's, would re-couple the capability across the whole test suite.
#[derive(Clone, Debug)]
pub(super) struct Reflinks {
    works: Arc<AtomicBool>,
}

/// What each copy destination has been observed to support, so a second copy into the same tree
/// does not have to rediscover it.
static CAPABILITIES: LazyLock<Mutex<HashMap<Utf8PathBuf, Arc<AtomicBool>>>> = LazyLock::new(|| Mutex::new(HashMap::default()));

impl Reflinks {
    /// The capability every copy into `destination` shares.
    pub(super) fn for_destination(destination: &Utf8Path) -> Self {
        let mut known = CAPABILITIES.lock().unwrap_or_else(PoisonError::into_inner);
        let works = Arc::clone(
            known
                .entry(destination.to_owned())
                .or_insert_with(|| Arc::new(AtomicBool::new(true))),
        );

        Self { works }
    }

    /// A capability no other copy — and, in the tests, no other test — shares.
    ///
    /// Registering nothing is the point: a test that copies onto a destination whose filesystem
    /// cannot clone would otherwise decide which branch every later copy in the process takes, and
    /// the tests run in parallel, so which branch a test exercises would depend on the order the
    /// harness happened to schedule them in. Both branches copy correctly, so nothing failed — the
    /// coverage simply moved.
    #[cfg(test)]
    pub(super) fn isolated() -> Self {
        Self {
            works: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Whether a clone is worth attempting for the next file.
    pub(super) fn worth_trying(&self) -> bool {
        reflink_supported() && self.works.load(Ordering::Relaxed)
    }

    /// Records that this destination cannot clone, so the rest of the copy stops asking.
    pub(super) fn unsupported(&self) {
        self.works.store(false, Ordering::Relaxed);
    }
}

/// How much of a tree a copy takes.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CopyOptions {
    /// Whether files version control ignores are copied along with the rest.
    ///
    /// The escape hatch for a tree whose `.gitignore` excludes something the build genuinely reads
    /// — a generated module, a fixture — where editing a shared ignore file to suit one tool is
    /// not an option.
    pub(super) copy_ignored: bool,
}

/// Copies a source tree, skipping build output, version control and the scratch directory itself.
///
/// `skip` is the directory the copy is being written under when that sits inside the source tree,
/// which is the default arrangement; without it the copy would try to copy itself.
///
/// Test-only, and deliberately on an isolated capability: what these tests check is what ends up in
/// the destination, which is the same either way a file gets there.
#[cfg(test)]
pub(super) fn copy_tree(from: &Utf8Path, to: &Utf8Path, skip: &Utf8Path) -> Result<()> {
    copy_tree_with(from, to, skip, CopyOptions::default(), &Reflinks::isolated())
}

/// Copies a source tree, taking what a run asked for into account.
///
/// Untracked files that version control ignores are not copied unless [`CopyOptions::copy_ignored`]
/// says otherwise. A tree's own `.gitignore` describes exactly the files that are regenerable or
/// machine-local, and skipping them is usually the difference between copying a source tree and
/// copying a source tree plus everything ever built in it.
///
/// Tracked files are copied whatever the ignore rules say. Git itself treats an ignore rule as
/// advice about what to add, not about what to keep, so a tracked file matching one is still part
/// of the tree — and mutant discovery, which walks the tree rather than asking git, finds and
/// mutates it. Leaving it out of the copy would fail the build over a file the real tree has.
///
/// `reflinks` is the cloning capability of `to`; see [`Reflinks`] for why the caller supplies it
/// rather than this reading a process-wide one.
pub(super) fn copy_tree_with(from: &Utf8Path, to: &Utf8Path, skip: &Utf8Path, options: CopyOptions, reflinks: &Reflinks) -> Result<()> {
    fs::create_dir_all(to.as_std_path()).map_err(|cause| error!("could not create the scratch tree at `{to}`").caused_by(cause))?;

    let failure: Mutex<Option<Error>> = Mutex::new(None);

    let mut builder = WalkBuilder::new(from.as_std_path());

    let _builder = builder
        // A build reads `.cargo/config.toml`, `.rustfmt.toml` and friends, none of which are
        // hidden in any sense that matters here.
        .hidden(false)
        // Only ignore files inside the tree have any say. Reading them from parent directories
        // means a checkout nested under a directory whose `.gitignore` says `*` copies as nothing
        // at all, and the resulting empty tree fails the build for reasons nobody can see.
        .parents(false)
        // `.gitignore` describes what git would restore, which is only meaningful in something git
        // is actually tracking. Outside a repository the same file is a leftover.
        .require_git(true)
        .git_ignore(!options.copy_ignored)
        .git_exclude(!options.copy_ignored)
        // A user's global ignore file describes their machine, not this project, and a rule in it
        // would silently change what a shared tree copies to.
        .git_global(false)
        // `.ignore` is a search convention. It routinely excludes vendored or generated code that
        // a build genuinely needs.
        .ignore(false)
        // A link is recreated rather than followed, so there is nothing to descend into and no
        // cycle to guard against.
        .follow_links(false);

    let root = from.to_owned();
    let destination = to.to_owned();
    let excluded = skip.to_owned();

    builder.build_parallel().run(|| {
        let root = root.clone();
        let destination = destination.clone();
        let excluded = excluded.clone();
        let failure = &failure;
        let reflinks = reflinks.clone();

        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,

                // An unreadable directory or a broken entry is reported rather than skipped. A
                // file missing from the copy produces a build failure naming something unrelated,
                // which is far harder to act on than the permission error that caused it.
                Err(cause) => {
                    record(failure, error!("could not read the source tree").caused_by(cause));

                    return WalkState::Quit;
                }
            };

            let Some(source) = Utf8Path::from_path(entry.path()) else {
                record(
                    failure,
                    error!("`{}` is not valid UTF-8 and cannot be copied", entry.path().display()),
                );

                return WalkState::Quit;
            };

            // The walker yields the root itself first, which is the destination, not something to
            // put inside it.
            let Ok(relative) = source.strip_prefix(&root) else {
                return WalkState::Continue;
            };

            if relative.as_str().is_empty() {
                return WalkState::Continue;
            }

            if is_pruned(source, relative, &excluded) {
                return WalkState::Skip;
            }

            match copy_entry(source, &destination.join(relative), &reflinks) {
                Ok(()) => WalkState::Continue,
                Err(cause) => {
                    record(failure, cause);

                    WalkState::Quit
                }
            }
        })
    });

    match failure.into_inner() {
        Ok(Some(cause)) => return Err(cause),
        Ok(None) => {}

        // The lock is only ever held while recording a failure, so it can only be poisoned by a
        // panic in this crate, and the panic itself is the thing worth reporting.
        Err(poisoned) => {
            if let Some(cause) = poisoned.into_inner() {
                return Err(cause);
            }
        }
    }

    copy_tracked(from, to, skip, reflinks)
}

/// Copies the files git tracks that the walk left behind.
///
/// Only files an ignore rule hid are still missing at this point, which is a handful in the trees
/// where it happens and none at all in the rest, so each candidate is settled by a single stat
/// rather than by re-deriving what the walk decided.
fn copy_tracked(from: &Utf8Path, to: &Utf8Path, skip: &Utf8Path, reflinks: &Reflinks) -> Result<()> {
    let Some(tracked) = tracked_files(from)? else {
        return Ok(());
    };

    for relative in tracked {
        if is_pruned_anywhere(from, &relative, skip) {
            continue;
        }

        let source = from.join(&relative);
        let destination = to.join(&relative);

        if fs::symlink_metadata(destination.as_std_path()).is_ok() {
            continue;
        }

        // A file that is in the index but not on disk — staged for deletion, or a submodule's
        // directory standing in for a checkout that was never made — is not something the real
        // tree builds against either.
        if fs::symlink_metadata(source.as_std_path()).is_err() {
            continue;
        }

        copy_entry(&source, &destination, reflinks)?;
    }

    Ok(())
}

/// Every path git tracks under `root`, or `None` when there is no repository to ask.
///
/// Asking git rather than reading the index directly keeps this working for a worktree, a
/// submodule and a repository whose index format is newer than any library would understand. A
/// directory that is not a repository, or a machine with no git at all, gets `None` and the
/// ignore walk's answer stands on its own.
pub(super) fn tracked_files(root: &Utf8Path) -> Result<Option<Vec<Utf8PathBuf>>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root.as_std_path())
        .args(["ls-files", "-z"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok();

    let Some(output) = output else {
        return Ok(None);
    };

    if !output.status.success() {
        return Ok(None);
    }

    let mut tracked = Vec::new();

    for name in output.stdout.split(|byte| *byte == 0).filter(|name| !name.is_empty()) {
        let name = str::from_utf8(name)
            .map_err(|cause| error!("git reported a tracked path that is not valid UTF-8 in `{root}`").caused_by(cause))?;

        tracked.push(Utf8PathBuf::from(name));
    }

    Ok(Some(tracked))
}

/// Returns whether any directory on the way to `relative` is one the copy leaves out.
///
/// The walk prunes a directory and never looks inside it; a path named by git arrives whole, so
/// each of its ancestors has to be put to the same question.
fn is_pruned_anywhere(root: &Utf8Path, relative: &Utf8Path, excluded: &Utf8Path) -> bool {
    let mut prefix = Utf8PathBuf::new();

    for component in relative.components() {
        prefix.push(component);

        if is_pruned(&root.join(&prefix), &prefix, excluded) {
            return true;
        }
    }

    false
}

/// Records the first failure, which is the one reported.
///
/// Later failures are usually consequences of the first — a walk that hit an unreadable directory
/// tends to hit its siblings too — and the walk is stopping regardless.
fn record(failure: &Mutex<Option<Error>>, cause: Error) {
    if let Ok(mut held) = failure.lock()
        && held.is_none()
    {
        *held = Some(cause);
    }
}

/// Returns whether an entry and everything under it should be left out of the copy.
pub(super) fn is_pruned(source: &Utf8Path, relative: &Utf8Path, excluded: &Utf8Path) -> bool {
    if source == excluded {
        return true;
    }

    let Some(name) = relative.file_name() else {
        return false;
    };

    if VCS_DIRS.contains(&name) {
        return true;
    }

    // Build output is expensive to copy and regenerated anyway. At the top of the tree the name
    // settles it; deeper down it does not, since `src/target/` is an ordinary module directory, so
    // a nested one has to prove itself by carrying the tag cargo writes into every target
    // directory it owns.
    name == "target" && (relative.parent() == Some(Utf8Path::new("")) || source.join("CACHEDIR.TAG").as_std_path().exists())
}

/// Copies one entry, preserving what it is.
fn copy_entry(source: &Utf8Path, destination: &Utf8Path, reflinks: &Reflinks) -> Result<()> {
    let metadata = fs::symlink_metadata(source.as_std_path()).map_err(|cause| error!("could not read `{source}`").caused_by(cause))?;

    if metadata.is_dir() {
        return fs::create_dir_all(destination.as_std_path()).map_err(|cause| error!("could not create `{destination}`").caused_by(cause));
    }

    // The parallel walk visits each directory as its own entry and creates it there, so by the time
    // a file is reached its parent almost always exists — placing it straight away keeps the
    // per-file `create_dir_all` (a `mkdir` returning `EEXIST` and an `is_dir` `stat`) off the common
    // path. The walk has no ordering between a file and its directory, though, so one case is left:
    // a file reached first. Only then is the parent created and the copy retried once, which keeps
    // the parallel-walk safety the unconditional call had — `create_dir_all` is idempotent, so a
    // parent another thread finished in the meantime is not a conflict, and a parent that is a plain
    // file surfaces here as the same "could not create" the unconditional call raised.
    if place(&metadata, source, destination, reflinks).is_err() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent.as_std_path()).map_err(|cause| error!("could not create `{parent}`").caused_by(cause))?;
        }

        return place(&metadata, source, destination, reflinks);
    }

    Ok(())
}

/// Copies one non-directory entry — a symlink verbatim, anything else as a file — assuming its
/// parent already exists.
fn place(metadata: &fs::Metadata, source: &Utf8Path, destination: &Utf8Path, reflinks: &Reflinks) -> Result<()> {
    if metadata.is_symlink() {
        copy_symlink(source, destination)
    } else {
        copy_file(source, destination, reflinks)
    }
}

/// Recreates a symlink rather than copying what it points at.
///
/// Following the link instead would materialize its target inside the scratch tree, which for a
/// link pointing outside the workspace — a home directory, a data mount — means copying that
/// wholesale. The link is reproduced verbatim, including a relative or broken one, since a build
/// that worked with it in the original tree is the thing being reproduced.
fn copy_symlink(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    let target = fs::read_link(source.as_std_path()).map_err(|cause| error!("could not read the link `{source}`").caused_by(cause))?;

    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(&target, destination.as_std_path());

    #[cfg(windows)]
    let created = if source
        .parent()
        .map_or_else(|| target.is_dir(), |parent| parent.as_std_path().join(&target).is_dir())
    {
        std::os::windows::fs::symlink_dir(&target, destination.as_std_path())
    } else {
        std::os::windows::fs::symlink_file(&target, destination.as_std_path())
    };

    created.map_err(|cause| error!("could not recreate the link `{destination}`").caused_by(cause))
}

/// Copies one file, cloning it if the filesystem can.
fn copy_file(source: &Utf8Path, destination: &Utf8Path, reflinks: &Reflinks) -> Result<()> {
    if reflinks.worth_trying() {
        match reflink_copy::reflink(source.as_std_path(), destination.as_std_path()) {
            Ok(()) => {
                freshen(destination);

                return Ok(());
            }
            // A missing parent is the parallel walk reaching a file before its directory, not a
            // filesystem that cannot clone. Surface it so `copy_entry` creates the parent and
            // retries, and leave the latch alone: one such race must not force the rest of the copy
            // onto the byte-for-byte path.
            Err(cause) if cause.kind() == ErrorKind::NotFound => {
                return Err(error!("could not copy `{source}` to `{destination}`").caused_by(cause));
            }
            Err(_unsupported) => {
                reflinks.unsupported();

                // A failed clone can leave a partial destination behind; remove it so the fallback
                // starts from the caller's precondition that no destination entry exists.
                let _removed = fs::remove_file(destination.as_std_path());
            }
        }
    }

    let _bytes = fs::copy(source.as_std_path(), destination.as_std_path())
        .map_err(|cause| error!("could not copy `{source}` to `{destination}`").caused_by(cause))?;

    Ok(())
}

/// Whether cloning is worth trying on this platform at all.
///
/// On musl the copy syscall the crate reaches for is not the standard one, and asking for a clone
/// there fails in ways that are not worth distinguishing from a filesystem that cannot do it.
const fn reflink_supported() -> bool {
    !cfg!(target_env = "musl")
}

/// Stamps a cloned file with the current time.
///
/// A clone preserves the source's modification time on some platforms, which leaves a fresh copy
/// looking arbitrarily old. On macOS that is not merely cosmetic: the system prunes files under
/// `/var/folders` once they pass three days, so a scratch tree cloned from an old checkout can
/// have files deleted out from under a run that is still using them.
fn freshen(destination: &Utf8Path) {
    if let Ok(file) = File::options().write(true).open(destination.as_std_path()) {
        let _stamped = file.set_times(FileTimes::new().set_modified(SystemTime::now()));
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    fn tree() -> (tempfile::TempDir, Utf8PathBuf, Utf8PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let from = Utf8PathBuf::from_path_buf(temporary.path().join("from")).unwrap();
        let to = Utf8PathBuf::from_path_buf(temporary.path().join("to")).unwrap();

        fs::create_dir_all(from.as_std_path()).unwrap();

        (temporary, from, to)
    }

    /// Runs one git command in `root`, reporting whether it worked.
    ///
    /// The tests that need a repository are skipped rather than failed on a machine without git,
    /// since what they cover is a git behaviour and there is nothing to check without it.
    fn git(root: &Utf8Path, arguments: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(root.as_std_path())
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Builds a repository holding one tracked and one untracked file, both matching an ignore
    /// rule. Returns `false` when git is unavailable.
    fn ignored_repository(from: &Utf8Path) -> bool {
        if !git(from, &["init"]) {
            return false;
        }

        fs::write(from.join(".gitignore").as_std_path(), "**/[Bb]in/*\n").unwrap();
        fs::create_dir_all(from.join("src").join("bin").as_std_path()).unwrap();
        fs::write(from.join("src").join("bin").join("helper.rs").as_std_path(), "fn tracked() {}").unwrap();
        fs::write(from.join("src").join("bin").join("scratch.rs").as_std_path(), "fn untracked() {}").unwrap();

        git(from, &["add", "-f", "src/bin/helper.rs"])
    }

    /// A file git tracks is part of the tree whatever an ignore rule says about it, and mutant
    /// discovery walks the tree rather than asking git, so leaving it out of the copy fails the
    /// build over a file the real tree has.
    #[test]
    fn a_tracked_file_is_copied_even_when_an_ignore_rule_matches_it() {
        let (_temporary, from, to) = tree();

        if !ignored_repository(&from) {
            return;
        }

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert_eq!(
            fs::read_to_string(to.join("src").join("bin").join("helper.rs").as_std_path()).unwrap(),
            "fn tracked() {}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_tracked_ignored_file_with_a_non_utf8_name_is_reported() {
        use std::os::unix::ffi::OsStrExt as _;

        let (_temporary, from, to) = tree();

        if !git(&from, &["init"]) {
            return;
        }

        let name = std::ffi::OsStr::from_bytes(b"ignored-\xff.rs");
        fs::write(from.join(".gitignore").as_std_path(), "ignored-*\n").expect("ignore rule");
        fs::write(from.as_std_path().join(name), "fn tracked() {}").expect("tracked file");

        let added = Command::new("git")
            .arg("-C")
            .arg(from.as_std_path())
            .args(["add", "-f", "--"])
            .arg(name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !added {
            return;
        }

        let error = copy_tree(&from, &to, Utf8Path::new("/nowhere")).expect_err("the path cannot be represented in the scratch tree");

        assert!(error.to_string().contains("not valid UTF-8"), "{error}");
    }

    #[test]
    fn an_untracked_ignored_file_is_not_copied() {
        let (_temporary, from, to) = tree();

        if !ignored_repository(&from) {
            return;
        }

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert!(!to.join("src").join("bin").join("scratch.rs").as_std_path().exists());
    }

    #[test]
    fn copying_ignored_files_takes_the_untracked_ones_too() {
        let (_temporary, from, to) = tree();

        if !ignored_repository(&from) {
            return;
        }

        copy_tree_with(
            &from,
            &to,
            Utf8Path::new("/nowhere"),
            CopyOptions { copy_ignored: true },
            &Reflinks::isolated(),
        )
        .unwrap();

        assert!(to.join("src").join("bin").join("helper.rs").as_std_path().exists());
        assert_eq!(
            fs::read_to_string(to.join("src").join("bin").join("scratch.rs").as_std_path()).unwrap(),
            "fn untracked() {}"
        );
    }

    /// Outside a repository there is no index to consult and no ignore file worth obeying, so
    /// everything is copied and asking git about it is not allowed to fail the copy.
    #[test]
    fn a_directory_that_is_not_a_repository_is_copied_whole() {
        let (_temporary, from, to) = tree();

        fs::write(from.join(".gitignore").as_std_path(), "**/[Bb]in/*\n").unwrap();
        fs::create_dir_all(from.join("src").join("bin").as_std_path()).unwrap();
        fs::write(from.join("src").join("bin").join("helper.rs").as_std_path(), "fn f() {}").unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert!(to.join("src").join("bin").join("helper.rs").as_std_path().exists());
    }

    /// A tracked file under a directory the copy prunes — build output, or the scratch tree itself
    /// — is still left out; being in the index says nothing about whether copying it is safe.
    #[test]
    fn a_tracked_file_under_a_pruned_directory_is_still_skipped() {
        assert!(is_pruned_anywhere(
            Utf8Path::new("/workspace"),
            Utf8Path::new("target/keep.rs"),
            Utf8Path::new("/workspace/scratch"),
        ));

        assert!(is_pruned_anywhere(
            Utf8Path::new("/workspace"),
            Utf8Path::new("scratch/tree/keep.rs"),
            Utf8Path::new("/workspace/scratch"),
        ));

        assert!(!is_pruned_anywhere(
            Utf8Path::new("/workspace"),
            Utf8Path::new("src/keep.rs"),
            Utf8Path::new("/workspace/scratch"),
        ));
    }

    /// Nothing outside a repository is tracked, and the answer has to be that rather than an
    /// error: a source tree that is not under version control is an ordinary thing to mutate.
    #[test]
    fn asking_a_non_repository_for_its_tracked_files_yields_nothing() {
        let (_temporary, from, _to) = tree();

        assert!(tracked_files(&from).expect("not being a repository is not an error").is_none());
    }

    #[test]
    fn build_output_and_version_control_are_skipped() {
        let (_temporary, from, to) = tree();

        for directory in ["src", "target", ".git", ".jj", "_darcs"] {
            fs::create_dir_all(from.join(directory).as_std_path()).unwrap();
            fs::write(from.join(directory).join("f").as_std_path(), "x").unwrap();
        }

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert!(to.join("src").join("f").as_std_path().exists());

        for skipped in ["target", ".git", ".jj", "_darcs"] {
            assert!(!to.join(skipped).as_std_path().exists(), "{skipped} was copied");
        }
    }

    #[test]
    fn a_nested_target_module_survives() {
        // `src/target/` is an ordinary module name. Only a directory carrying cargo's tag is
        // build output.
        let (_temporary, from, to) = tree();

        fs::create_dir_all(from.join("src").join("target").as_std_path()).unwrap();
        fs::write(from.join("src").join("target").join("mod.rs").as_std_path(), "fn f() {}").unwrap();

        fs::create_dir_all(from.join("nested").join("target").as_std_path()).unwrap();
        fs::write(from.join("nested").join("target").join("CACHEDIR.TAG").as_std_path(), "").unwrap();
        fs::write(from.join("nested").join("target").join("junk").as_std_path(), "x").unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert!(to.join("src").join("target").join("mod.rs").as_std_path().exists());
        assert!(!to.join("nested").join("target").join("junk").as_std_path().exists());
    }

    #[test]
    fn nested_directories_are_recreated() {
        let (_temporary, from, to) = tree();

        fs::create_dir_all(from.join("a").join("b").join("c").as_std_path()).unwrap();
        fs::write(from.join("a").join("b").join("c").join("deep.rs").as_std_path(), "fn f() {}").unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert_eq!(
            fs::read_to_string(to.join("a").join("b").join("c").join("deep.rs").as_std_path()).unwrap(),
            "fn f() {}"
        );
    }

    #[test]
    fn the_scratch_directory_is_not_copied_into_itself() {
        let (_temporary, from, to) = tree();
        let skip = from.join("scratch");

        fs::create_dir_all(skip.as_std_path()).unwrap();
        fs::write(skip.join("f").as_std_path(), "x").unwrap();
        fs::create_dir_all(from.join("src").as_std_path()).unwrap();

        copy_tree(&from, &to, &skip).unwrap();

        assert!(!to.join("scratch").as_std_path().exists());
        assert!(to.join("src").as_std_path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_recreated_rather_than_followed() {
        // Following it would materialize whatever it points at, which for a link out of the
        // workspace means copying an arbitrary part of the filesystem.
        let (_temporary, from, to) = tree();
        let outside = from.parent().unwrap().join("outside");

        fs::create_dir_all(outside.as_std_path()).unwrap();
        fs::write(outside.join("secret").as_std_path(), "x").unwrap();

        std::os::unix::fs::symlink(outside.as_std_path(), from.join("link").as_std_path()).unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        let copied = to.join("link");

        assert!(fs::symlink_metadata(copied.as_std_path()).unwrap().is_symlink());
        assert_eq!(fs::read_link(copied.as_std_path()).unwrap(), outside.as_std_path());
    }

    #[cfg(windows)]
    #[test]
    fn a_relative_directory_symlink_is_recreated_as_a_directory_link() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(temporary.path()).expect("utf8");
        let from = root.join("from");
        let to = root.join("to");

        fs::create_dir_all(from.join("sub").as_std_path()).expect("sub");
        fs::create_dir_all(from.join("shared").as_std_path()).expect("shared");
        fs::write(from.join("shared/marker").as_std_path(), "present").expect("marker");

        // Windows stores the target verbatim, and the object manager requires backslashes.
        if let Err(cause) = std::os::windows::fs::symlink_dir(r"..\shared", from.join("sub/link").as_std_path()) {
            if cause.kind() == ErrorKind::PermissionDenied {
                return;
            }

            panic!("create source link: {cause}");
        }

        copy_tree(&from, &to, &to).expect("copy");

        let copied = to.join("sub/link");
        assert!(fs::symlink_metadata(copied.as_std_path()).expect("metadata").is_symlink());
        assert!(copied.as_std_path().is_dir(), "the copied directory link is not traversable");

        // Reading through the link proves it resolves inside the copied tree.
        let through = fs::read_to_string(copied.join("marker").as_std_path()).expect("read through the copied link");

        assert_eq!(through, "present");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_does_not_hang_the_copy() {
        // The old hand-rolled walk followed links and needed a depth cap to survive this.
        let (_temporary, from, to) = tree();

        fs::create_dir_all(from.join("a").as_std_path()).unwrap();
        std::os::unix::fs::symlink(from.as_std_path(), from.join("a").join("loop").as_std_path()).unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert!(fs::symlink_metadata(to.join("a").join("loop").as_std_path()).unwrap().is_symlink());
    }

    #[test]
    fn a_deep_tree_is_copied_whole() {
        // The old walk stopped at 64 levels and returned success, losing everything below.
        let (_temporary, from, to) = tree();
        let mut deep = from.clone();

        for _level in 0..80 {
            deep = deep.join("d");
        }

        fs::create_dir_all(deep.as_std_path()).unwrap();
        fs::write(deep.join("bottom.rs").as_std_path(), "fn f() {}").unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        let landed = to.join(deep.strip_prefix(&from).unwrap());

        assert!(landed.join("bottom.rs").as_std_path().exists());
    }

    #[test]
    fn an_empty_directory_is_preserved() {
        // A build script can expect a directory to exist without anything being in it.
        let (_temporary, from, to) = tree();

        fs::create_dir_all(from.join("empty").as_std_path()).unwrap();

        copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap();

        assert!(to.join("empty").as_std_path().is_dir());
    }

    #[test]
    fn a_missing_source_tree_is_reported() {
        let (_temporary, from, to) = tree();
        let missing = from.join("absent");

        let cause = copy_tree(&missing, &to, Utf8Path::new("/nowhere")).unwrap_err();

        assert!(cause.to_string().contains("could not read the source tree"), "{cause}");
    }

    #[test]
    fn a_destination_entry_that_cannot_be_replaced_is_reported() {
        let (_temporary, from, to) = tree();

        fs::write(from.join("file").as_std_path(), "source").unwrap();
        fs::create_dir_all(to.join("file").as_std_path()).unwrap();

        let cause = copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap_err();

        // The parallel walk records the first failed copy rather than continuing and reporting a
        // later build error about whatever was missing from the scratch tree.
        assert!(cause.to_string().contains("could not copy"), "{cause}");
    }

    /// When several entries fail at nearly the same time — plausible on a multi-core machine, since
    /// the walk copies in parallel — only the first failure recorded is kept and reported; every
    /// later one is a duplicate of essentially the same problem, and showing several at once would
    /// bury the one useful message under repeats of it.
    #[test]
    fn only_the_first_of_several_concurrent_failures_is_kept() {
        let (_temporary, from, to) = tree();

        fs::create_dir_all(to.as_std_path()).unwrap();

        // Enough independent blocked entries, spread across their own directories, that a parallel
        // walk has a real chance of two of them failing before either sees the other quit.
        for index in 0..32 {
            let name = format!("blocked-{index}");

            fs::create_dir_all(from.join(&name).as_std_path()).unwrap();
            fs::write(from.join(&name).join("file").as_std_path(), "source").unwrap();
            fs::create_dir_all(to.join(&name).join("file").as_std_path()).unwrap();
        }

        let cause = copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap_err();

        assert!(cause.to_string().contains("could not copy"), "{cause}");
    }

    /// `record` keeps only the earliest failure so a user chasing a build error sees the actual
    /// cause rather than whichever unrelated sibling entry the parallel walk happened to reach
    /// last; calling it directly, rather than hoping a race lands the right way, is the only way
    /// to prove that second and later calls are genuinely no-ops rather than merely untested.
    #[test]
    fn a_second_recorded_failure_does_not_replace_the_first() {
        let failure: Mutex<Option<Error>> = Mutex::new(None);

        record(&failure, error!("first failure"));
        record(&failure, error!("second failure"));

        let held = failure.into_inner().unwrap();
        let cause = held.expect("a failure was recorded");

        assert!(cause.to_string().contains("first failure"), "{cause}");
    }

    #[test]
    fn pruning_an_entry_without_a_file_name_is_not_a_match() {
        // The root entry has no real file name after it is stripped, and must not be mistaken for
        // a version-control or target directory.
        assert!(!is_pruned(
            Utf8Path::new("/workspace"),
            Utf8Path::new(""),
            Utf8Path::new("/elsewhere"),
        ));
    }

    #[test]
    fn freshening_a_missing_file_is_harmless() {
        let (_temporary, _from, to) = tree();
        let file = to.join("copied");

        fs::create_dir_all(to.as_std_path()).unwrap();
        fs::write(file.as_std_path(), "x").unwrap();
        freshen(&file);
        fs::remove_file(file.as_std_path()).unwrap();
        freshen(&file);

        // Freshening is best-effort metadata repair after a reflink; it must never turn a
        // successful copy into an error just because timestamps cannot be changed.
        assert!(!file.as_std_path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_source_entry_is_reported() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let (_temporary, from, to) = tree();
        let name = OsString::from_vec(b"bad-\xff".to_vec());

        fs::write(from.as_std_path().join(name), "x").unwrap();

        let cause = copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap_err();

        // Paths in reports and manifests are UTF-8; a lossy copy would make the later error point
        // at a name the user cannot match back to the source tree.
        assert!(cause.to_string().contains("not valid UTF-8"), "{cause}");
    }

    /// The scratch tree's own destination has to be creatable before anything is copied into it; if
    /// a plain file already sits where a directory of that name is needed, the failure has to be
    /// reported up front rather than surfacing later as a confusing per-file copy error.
    #[test]
    fn a_destination_root_blocked_by_a_file_is_reported() {
        let (_temporary, from, to) = tree();

        // `to` is not created by `tree()`; putting a plain file there instead means the very first
        // thing `copy_tree` does, creating its own destination, has nowhere to go.
        fs::write(to.as_std_path(), "not a directory").unwrap();

        let cause = copy_tree(&from, &to, Utf8Path::new("/nowhere")).unwrap_err();

        assert!(cause.to_string().contains("could not create the scratch tree"), "{cause}");
    }

    /// A source entry the walker named but which vanished before it could be copied — a file
    /// deleted out from under a concurrent build, say — is reported by name rather than treated as
    /// though it had never existed, so the missing file in the resulting build failure can be
    /// traced back to a real cause instead of an unrelated compile error.
    #[test]
    fn copying_an_entry_that_no_longer_exists_is_reported() {
        let (_temporary, from, to) = tree();
        let gone = from.join("was-here");

        let cause = copy_entry(&gone, &to.join("was-here"), &Reflinks::isolated()).unwrap_err();

        assert!(cause.to_string().contains("could not read"), "{cause}");
    }

    /// Copying a directory whose destination cannot be created — because a plain file already
    /// occupies that name — has to fail with a named error rather than silently doing nothing,
    /// which would leave the scratch tree missing a directory the build expects to find.
    #[test]
    fn copying_a_directory_blocked_by_a_file_is_reported() {
        let (_temporary, from, to) = tree();

        fs::create_dir_all(from.join("adir").as_std_path()).unwrap();
        fs::create_dir_all(to.as_std_path()).unwrap();
        fs::write(to.join("adir").as_std_path(), "blocking file").unwrap();

        let cause = copy_entry(&from.join("adir"), &to.join("adir"), &Reflinks::isolated()).unwrap_err();

        assert!(cause.to_string().contains("could not create"), "{cause}");
    }

    /// A file's parent directory is created on demand rather than assumed to already exist, since
    /// the walk copies entries in parallel with no ordering between a file and its own directory.
    /// If that parent cannot be created — a plain file sits where it belongs — the failure has to
    /// be reported rather than silently dropping the file from the copy.
    #[test]
    fn a_files_parent_blocked_by_a_file_is_reported() {
        let (_temporary, from, to) = tree();

        fs::write(from.join("leaf").as_std_path(), "source").unwrap();
        fs::create_dir_all(to.as_std_path()).unwrap();
        fs::write(to.join("blocker").as_std_path(), "blocking file").unwrap();

        let cause = copy_entry(&from.join("leaf"), &to.join("blocker").join("leaf"), &Reflinks::isolated()).unwrap_err();

        assert!(cause.to_string().contains("could not create"), "{cause}");
    }

    /// A file whose parent has not been created yet is still copied: the parallel walk has no
    /// ordering between a file and its directory, so the common-path copy that skips the per-file
    /// `create_dir_all` must fall back to creating the parent and retrying rather than dropping the
    /// file. Without the retry a file reached before its directory would vanish from the copy.
    #[test]
    fn a_files_missing_parent_is_created_on_demand() {
        let (_temporary, from, to) = tree();

        fs::write(from.join("leaf").as_std_path(), "source").unwrap();
        fs::create_dir_all(to.as_std_path()).unwrap();

        // `to/nested` deliberately does not exist yet.
        copy_entry(&from.join("leaf"), &to.join("nested").join("leaf"), &Reflinks::isolated()).unwrap();

        assert_eq!(fs::read_to_string(to.join("nested").join("leaf").as_std_path()).unwrap(), "source");
    }

    /// A symlink's target has to be read before it can be recreated, and a source that no longer
    /// names a link at all — removed, or never one to begin with, between the walk seeing it and
    /// the copy reaching it — must be reported rather than silently skipped, which would leave a
    /// gap in the copy nothing else explains.
    #[cfg(unix)]
    #[test]
    fn a_symlink_whose_target_can_no_longer_be_read_is_reported() {
        let (_temporary, from, to) = tree();
        let missing = from.join("not-a-link");

        let cause = copy_symlink(&missing, &to.join("not-a-link")).unwrap_err();

        assert!(cause.to_string().contains("could not read the link"), "{cause}");
    }

    /// Recreating a symlink at a destination that already exists has to fail rather than silently
    /// leaving whatever was there, since two entries in the walk could otherwise race to claim the
    /// same path and only one error would ever be visible if this were not checked.
    #[cfg(unix)]
    #[test]
    fn recreating_a_symlink_over_an_existing_entry_is_reported() {
        let (_temporary, from, to) = tree();
        let target = from.join("target-file");

        fs::write(target.as_std_path(), "x").unwrap();
        std::os::unix::fs::symlink(target.as_std_path(), from.join("link").as_std_path()).unwrap();
        fs::create_dir_all(to.as_std_path()).unwrap();
        fs::write(to.join("link").as_std_path(), "already here").unwrap();

        let cause = copy_symlink(&from.join("link"), &to.join("link")).unwrap_err();

        assert!(cause.to_string().contains("could not recreate the link"), "{cause}");
    }

    /// A destination that cannot clone must not decide for a destination that can.
    ///
    /// A run writes to more than one place — the default scratch tree under the user's cache
    /// directory, and a `--cache-dir` that routinely names another mount — and the capability used
    /// to be one process-wide latch. The first failure anywhere sent every later copy in the
    /// process down the byte-for-byte path, on filesystems that clone perfectly well. Nothing was
    /// copied wrongly, which is exactly why it went unnoticed: it only ever cost time.
    #[test]
    fn a_destination_that_cannot_clone_does_not_disable_cloning_for_another() {
        let (_temporary, from, to) = tree();
        let elsewhere = to.parent().expect("the fixture destination has a parent").join("elsewhere");

        let unsupported = Reflinks::for_destination(&to);
        let other = Reflinks::for_destination(&elsewhere);

        unsupported.unsupported();

        assert!(!unsupported.worth_trying(), "the failing destination must stop asking");
        assert_eq!(
            other.worth_trying(),
            reflink_supported(),
            "one destination's failure must not answer for another"
        );

        // A second copy into the same destination inherits what the first one learned there, which
        // is the whole point of remembering it at all.
        assert!(!Reflinks::for_destination(&to).worth_trying());

        // Both destinations still copy, whichever branch they take.
        fs::write(from.join("file.rs").as_std_path(), "fn f() {}").unwrap();
        copy_tree_with(&from, &to, Utf8Path::new("/nowhere"), CopyOptions::default(), &unsupported).unwrap();
        copy_tree_with(&from, &elsewhere, Utf8Path::new("/nowhere"), CopyOptions::default(), &other).unwrap();

        assert_eq!(fs::read_to_string(to.join("file.rs").as_std_path()).unwrap(), "fn f() {}");
        assert_eq!(fs::read_to_string(elsewhere.join("file.rs").as_std_path()).unwrap(), "fn f() {}");
    }

    /// A test that meets an unsupported destination must not change which branch another test runs.
    ///
    /// The capability was a process-global one-way latch, and the tests run in parallel: one test
    /// copying onto a filesystem without cloning silently moved every later test in that process
    /// onto the fallback path. Both branches copy correctly, so nothing failed — but which branch
    /// any given test covered depended on the order the harness happened to schedule them in, which
    /// is not a property a coverage number can be read against.
    #[test]
    fn an_isolated_capability_shares_nothing_with_the_registry_or_another_test() {
        let (_temporary, _from, to) = tree();

        let registered = Reflinks::for_destination(&to);
        let mine = Reflinks::isolated();
        let theirs = Reflinks::isolated();

        mine.unsupported();

        assert!(!mine.worth_trying(), "a test's own capability is its own to trip");
        assert_eq!(theirs.worth_trying(), reflink_supported(), "another test must be unaffected");
        assert_eq!(
            registered.worth_trying(),
            reflink_supported(),
            "an isolated capability must not reach the shared registry"
        );

        // And the traffic does not flow the other way either: tripping the registered capability
        // for this destination leaves both isolated ones as they were.
        registered.unsupported();

        assert_eq!(theirs.worth_trying(), reflink_supported());
    }
}
