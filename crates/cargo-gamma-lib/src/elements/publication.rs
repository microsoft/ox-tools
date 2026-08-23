// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writing a file atomically: whole or not at all.
//!
//! Reports, the generated configuration and the source files `suppress` and `unsuppress` rewrite
//! are all published through here, so a failed or interrupted write can never leave a truncated
//! file under the destination name.

#[cfg(test)]
use core::cell::{Cell, RefCell};
use core::hash::{BuildHasher as _, Hasher as _};
use core::sync::atomic::{AtomicU64, Ordering};
use std::collections::hash_map::RandomState;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Write as _};

use camino::{Utf8Path, Utf8PathBuf};

use crate::Result;
use crate::error::error;

/// Writes a file whole or not at all, creating parent directories as needed.
///
/// The contents go to a sibling of the destination and are renamed onto it once they are all
/// there and durable, and the rename itself is made durable in turn, so a crash, a kill or a full
/// disk part-way through leaves the previous file rather than a truncated one — and a machine that
/// loses power after this returns comes back with the new file rather than with neither. That
/// matters because the reader these outputs exist for is a CI
/// job parsing the JSON report, which cannot tell a truncated file from a short one — it sees a
/// name it expects, holding something that is not valid JSON, and reports a broken run instead of
/// a failed write. It matters more for the source files `suppress` and `unsuppress` rewrite, where
/// the truncated file is not a report nobody wrote by hand but the user's own code.
///
/// The temporary is a sibling rather than a file in the system temporary directory so that the
/// rename stays within one filesystem, which is what makes it atomic; and it carries a suffix that
/// is unique to each write across the whole filesystem, so that overlapping runs cannot collide.
/// See `scratch_path` for why the process id alone is not enough.
///
/// A destination that already exists keeps the permissions it has when the contents are finished,
/// and a destination reached through a symlink is replaced through the link rather than in place of
/// it. Both are decisions the user made about their own file, and replacing its contents is no
/// reason to undo either. The permissions are a snapshot taken immediately before the rename rather
/// than a lock: nothing here excludes a concurrent `chmod`, so a mode set between that snapshot and
/// the rename is still lost.
///
/// # Errors
///
/// Returns the reason when the directory, the temporary file, the rename or the durability of
/// either fails. A temporary left behind by a failed write is removed, because the alternative is
/// litter beside the report with no one to clean it up.
pub fn write(path: &Utf8Path, contents: &str) -> Result<()> {
    // Resolved before anything is staged, so that the rename lands on what the name points at and
    // the staging file is a sibling of the file the rename will actually touch. A dangling link
    // still has a destination: creating through it must preserve the link rather than replace it.
    let destination = crate::paths::physical(path)?;
    create_parents(&destination)?;

    replace(path, &destination, write_bytes(contents))
}

/// Writes a file whole or not at all by streaming it through `fill`, creating parent directories as
/// needed.
///
/// The streaming counterpart to [`write`], for a document large enough that holding the finished
/// string on top of what produced it is the memory spike worth avoiding — the report and the
/// self-contained page. `fill` writes into the staging file; the atomic write-then-rename contract,
/// the durability barrier and the error context are exactly [`write`]'s.
///
/// # Errors
///
/// Returns the reason when the directory, the temporary file, `fill`, the rename or the durability
/// of either fails. A temporary left behind by a failed write is removed.
pub(crate) fn write_streamed(path: &Utf8Path, fill: impl FnOnce(&mut dyn io::Write) -> io::Result<()>) -> Result<()> {
    let destination = crate::paths::physical(path)?;
    create_parents(&destination)?;

    replace(path, &destination, fill)
}

/// Atomically replaces `path` only when it still holds `expected`.
///
/// The lock belongs to the original workspace's stable cargo-gamma cache, so every command editing
/// that workspace shares one lock domain even when its reusable build state was redirected with
/// `--cache-dir`.
///
/// [`Publication::Conflict`] means the destination did not hold `expected` at the final comparison;
/// no destination bytes were replaced. That comparison happens after staging. Portable Rust has no
/// inode- or generation-aware compare-and-replace primitive, so a non-cooperating process can
/// replace the destination in the syscall interval between the comparison and rename and be
/// overwritten. The directory lock closes that interval only for writers using this API.
pub(crate) fn write_if_unchanged(workspace: &Utf8Path, path: &Utf8Path, expected: Option<&str>, contents: &str) -> Result<Publication> {
    let destination = crate::paths::physical(path)?;
    create_parents(&destination)?;
    let _lock = crate::exec::claim_workspace(workspace)?;
    let scratch = scratch_path(&destination);

    stage(&scratch, write_bytes(contents), Some(&destination)).map_err(|cause| discard(&scratch, path, cause))?;
    before_publication(&scratch);

    if !matches_contents(&destination, expected).map_err(|cause| error!("could not check `{path}` before replacing it").caused_by(cause))? {
        remove_staging(&scratch, path)?;
        return Ok(Publication::Conflict);
    }

    after_comparison(&destination);
    fs::rename(scratch.as_std_path(), destination.as_std_path()).map_err(|cause| discard(&scratch, path, cause))?;

    match published(&destination) {
        Ok(()) => Ok(Publication::Published),
        Err(cause) => Ok(Publication::PublishedUndurable(error!("could not write `{path}`").caused_by(cause))),
    }
}

/// Removes `path` only when it still holds `expected`.
///
/// This is the rollback counterpart to [`write_if_unchanged`]. It shares the same lock and final
/// generation check, so a failed transaction cannot delete a later successful writer's generation.
pub(crate) fn remove_if_unchanged(workspace: &Utf8Path, path: &Utf8Path, expected: &str) -> Result<Publication> {
    let destination = crate::paths::physical(path)?;
    let _lock = crate::exec::claim_workspace(workspace)?;

    before_publication(&destination);

    if !matches_contents(&destination, Some(expected))
        .map_err(|cause| error!("could not check `{path}` before removing it").caused_by(cause))?
    {
        return Ok(Publication::Conflict);
    }

    fs::remove_file(destination.as_std_path()).map_err(|cause| error!("could not remove `{path}`").caused_by(cause))?;

    match published(&destination) {
        Ok(()) => Ok(Publication::Published),
        Err(cause) => Ok(Publication::PublishedUndurable(
            error!("could not remove `{path}`").caused_by(cause),
        )),
    }
}

/// Whether a conditional publication reached the destination name.
///
/// A directory sync follows the rename, so it can fail after the new generation is visible.
/// Callers that compensate source edits must record that generation before propagating
/// [`Self::PublishedUndurable`], otherwise their rollback loses track of an edit that happened.
#[derive(Debug)]
pub(crate) enum Publication {
    /// The validated generation was gone before the rename or removal.
    Conflict,

    /// The new generation was published and its directory was synced.
    Published,

    /// The new generation was published but its directory could not be synced.
    PublishedUndurable(crate::error::Error),
}

/// Replaces the resolved destination after all checks that protect this publication have passed.
fn replace(path: &Utf8Path, destination: &Utf8Path, fill: impl FnOnce(&mut dyn io::Write) -> io::Result<()>) -> Result<()> {
    let scratch = scratch_path(destination);

    stage(&scratch, fill, Some(destination)).map_err(|cause| discard(&scratch, path, cause))?;
    before_publication(&scratch);

    fs::rename(scratch.as_std_path(), destination.as_std_path()).map_err(|cause| discard(&scratch, path, cause))?;

    published(destination).map_err(|cause| error!("could not write `{path}`").caused_by(cause))
}

/// Writes a file at a name that must be free, whole or not at all, creating parent directories as
/// needed.
///
/// Returns whether it was written: `false` means the name was already taken and nothing on disk
/// was touched, which is a decision for the caller rather than a failure — the file that is there
/// may be one somebody wrote by hand.
///
/// Publishing is a link from the staged copy rather than a rename onto the name, because a rename
/// replaces whatever is already there and the whole point here is that it must not. It is also not
/// a create-then-write into the final name: a write that fails part-way through would leave a
/// corrupt file under the name a retry then refuses to overwrite, which is the failure this exists
/// to rule out. Linking publishes bytes that are already complete and already durable, in one step
/// that either takes the free name or reports that it was not free — and a failure before that
/// step leaves no file under the final name at all. The new directory entry is then made durable
/// in its own right, because this is the path that prints "Wrote `gamma.toml`" and a file
/// the user was told about has to still be there after a power cut — on Unix, which is as far as
/// `published` can promise.
///
/// # Errors
///
/// Returns the reason when the directory, the temporary file, the link or the durability of the
/// link fails.
pub fn publish(path: &Utf8Path, contents: &str) -> Result<bool> {
    create_parents(path)?;

    let scratch = scratch_path(path);

    stage(&scratch, write_bytes(contents), None).map_err(|cause| discard(&scratch, path, cause))?;

    let linked = match fs::hard_link(scratch.as_std_path(), path.as_std_path()) {
        Ok(()) => true,

        Err(cause) if cause.kind() == ErrorKind::AlreadyExists => false,

        Err(cause) => return Err(discard(&scratch, path, cause)),
    };

    let durability = linked.then(|| published(path)).transpose();

    // Said out loud rather than swallowed: the file under the final name is correct either way, so
    // this is not a failed publish, but the leftover sits beside it under a hidden name that
    // nothing else will ever mention.
    if let Err(cause) = fs::remove_file(scratch.as_std_path()) {
        crate::notes::note(format!("`{scratch}` was left behind and could not be removed: {cause}"));
    }

    if let Err(cause) = durability {
        return Err(error!("could not write `{path}`").caused_by(cause));
    }

    Ok(linked)
}

/// Creates the directories `path` will be written into.
fn create_parents(path: &Utf8Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent.as_std_path()).map_err(|cause| error!("could not create `{parent}`").caused_by(cause))?;
    }

    Ok(())
}

/// Whether the path still names exactly the bytes its caller validated.
fn matches_contents(path: &Utf8Path, expected: Option<&str>) -> io::Result<bool> {
    match fs::read(path.as_std_path()) {
        Ok(actual) => Ok(expected.is_some_and(|expected| actual == expected.as_bytes())),
        Err(cause) if cause.kind() == ErrorKind::NotFound => Ok(expected.is_none()),
        Err(cause) => Err(cause),
    }
}

/// Removes an unpublishable staged file, preserving the conflict as the caller's result.
fn remove_staging(scratch: &Utf8Path, path: &Utf8Path) -> Result<()> {
    match fs::remove_file(scratch.as_std_path()) {
        Ok(()) => Ok(()),
        Err(cause) if cause.kind() == ErrorKind::NotFound => Ok(()),
        Err(cause) => Err(error!("`{path}` changed before it could be replaced, and `{scratch}` could not be removed").caused_by(cause)),
    }
}

/// Fills the staging file and makes it durable, taking `mode`'s permissions when there are any.
///
/// The contents are flushed to the device before the caller publishes them. Without that, the
/// rename or the link can reach the disk ahead of the bytes it names, and a machine that loses
/// power in between comes back with the new name over an empty file — the outcome staging exists
/// to rule out, reached by a different route.
///
/// The staging file is created exclusively rather than truncated into. A name that is already
/// taken is a name this call has no business writing through: it is either another process's
/// staging file — which truncating would corrupt from both ends — or something planted under a
/// predictable name for this process to write through, which `O_EXCL` also refuses to follow.
///
/// The destination's permissions are read and applied *after* the contents are written. Reading
/// them last is what keeps the snapshot as close to the rename as it can be without a lock, and
/// applying them last keeps the file out of the state Windows enforces on the next handle open
/// rather than retroactively — where marking the staging file read-only before writing it is a
/// hazard rather than the caution it is on Unix.
///
/// The contents are produced by `fill` writing into the staging file rather than handed over as a
/// finished string, so a caller with a large document — a report and the page that embeds it — can
/// stream it through a buffered writer instead of materializing the whole thing first. The buffer
/// is flushed before the permissions and the durability barrier, so every byte `fill` wrote is on
/// the file by the time it is synced.
fn stage(scratch: &Utf8Path, fill: impl FnOnce(&mut dyn io::Write) -> io::Result<()>, mode: Option<&Utf8Path>) -> io::Result<()> {
    let mut staging = File::create_new(scratch.as_std_path())?;

    {
        let mut writer = io::BufWriter::new(&mut staging);

        fill(&mut writer)?;
        writer.flush()?;
    }

    if let Some(permissions) = mode
        .and_then(|path| fs::metadata(path.as_std_path()).ok())
        .map(|metadata| metadata.permissions())
    {
        staging.set_permissions(permissions)?;
    }

    staging.sync_all()
}

/// Writes `contents` straight through, the [`stage`] filler for a caller that already holds the
/// finished bytes.
fn write_bytes(contents: &str) -> impl FnOnce(&mut dyn io::Write) -> io::Result<()> + '_ {
    move |writer| writer.write_all(contents.as_bytes())
}

/// Makes the appearance of `path` under its own name durable, not merely its contents.
///
/// `sync_all` on the staging file covers the bytes; the rename or the link that gives them their
/// final name is a change to the *parent directory*, and on XFS, on btrfs and on ext4 mounted
/// `data=writeback` that entry can be lost across a power cut even though every byte it names was
/// synced. The caller can report a successful write before that loss, so the directory entry is
/// part of the durability guarantee too.
///
/// **The guarantee is Unix-only, deliberately, and this is a no-op elsewhere.** Windows offers no
/// equivalent barrier that would cover the operations used here:
///
/// - `FlushFileBuffers` needs a handle opened for writing, and a directory cannot be opened that
///   way, so the Unix move of syncing the parent has no spelling on Windows at all.
/// - `MoveFileEx` has `MOVEFILE_WRITE_THROUGH`, which does not return "until the file is actually
///   moved on the disk" — but `std::fs::rename` calls `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`
///   alone, so nothing here requests it, and reaching it would mean an FFI rename of our own.
/// - Even that would cover only [`replace`]. [`publish`] takes its name with `CreateHardLink` and
///   the removal path with `DeleteFile`, and neither has a write-through flag. A barrier on one of
///   the three would leave the guarantee looking whole while two paths still lacked it, which is
///   worse than the honest gap.
///
/// So on Windows the bytes are durable and the directory entry naming them is at NTFS's discretion.
/// NTFS journals metadata, so the entry is far more likely to survive than on the Unix filesystems
/// named above, but that is a property of the filesystem rather than a promise this code obtained.
#[cfg(unix)]
fn published(path: &Utf8Path) -> io::Result<()> {
    #[cfg(test)]
    if take_directory_sync_failure() {
        return Err(io::Error::other("injected directory sync failure"));
    }

    let parent = path.parent().filter(|parent| !parent.as_str().is_empty());

    File::open(parent.unwrap_or_else(|| Utf8Path::new(".")).as_std_path())?.sync_all()
}

/// Always succeeds off Unix: see the Unix spelling for why there is nothing to ask for there.
#[cfg(all(not(unix), test))]
fn published(_path: &Utf8Path) -> io::Result<()> {
    if take_directory_sync_failure() {
        return Err(io::Error::other("injected directory sync failure"));
    }

    Ok(())
}

#[cfg(all(not(unix), not(test)))]
#[expect(clippy::unnecessary_wraps, reason = "the Unix spelling is fallible, and the two must agree")]
const fn published(_path: &Utf8Path) -> io::Result<()> {
    Ok(())
}

/// Removes the staging file and reports why the write it held could not be completed.
///
/// A failure to remove it is folded into the message rather than dropped. The leftover is beside
/// the destination under a name nothing else mentions, so a caller told only about the first
/// failure would never learn it is there.
fn discard(scratch: &Utf8Path, path: &Utf8Path, cause: io::Error) -> crate::error::Error {
    let failed = error!("could not write `{path}`").caused_by(cause);

    match fs::remove_file(scratch.as_std_path()) {
        Ok(()) => failed,

        Err(removal) if removal.kind() == ErrorKind::NotFound => failed,

        Err(removal) => error!("{failed}; and `{scratch}` could not be removed either: {removal}"),
    }
}

/// Where the contents of `path` are staged before being published onto it.
///
/// A per-process counter gives each call a different name, while random entropy separates pid
/// namespaces sharing a filesystem. A pid is unique only within its namespace, and two containers
/// sharing a bind-mounted workspace can both start their agent at pid 1. The exclusive create in
/// [`stage`] is a final defence against the astronomically unlikely cross-process collision.
pub(crate) fn scratch_path(path: &Utf8Path) -> Utf8PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[cfg(test)]
    if let Some(scratch) = TEST_SCRATCH.with(|next| next.borrow_mut().take()) {
        return scratch;
    }

    let name = path.file_name().unwrap_or("report");
    let invocation = NEXT.fetch_add(1, Ordering::Relaxed);

    // The counter gives every call in this process a distinct name. The random portion separates
    // pid namespaces sharing a filesystem, where two unrelated processes can both be pid 1 and
    // make the same sequence of calls.
    let mut hasher = RandomState::new().build_hasher();

    hasher.write_u32(std::process::id());
    hasher.write_u64(invocation);

    path.with_file_name(format!(".{name}.{}.{invocation}.{:016x}.tmp", std::process::id(), hasher.finish()))
}

#[cfg(test)]
type PublicationHook = Box<dyn FnOnce(&Utf8Path)>;

#[cfg(test)]
thread_local! {
    static BEFORE_PUBLICATION: RefCell<Option<PublicationHook>> = const { RefCell::new(None) };
    static AFTER_COMPARISON: RefCell<Option<PublicationHook>> = const { RefCell::new(None) };
    static TEST_SCRATCH: RefCell<Option<Utf8PathBuf>> = const { RefCell::new(None) };
    static DIRECTORY_SYNC_FAILURE: Cell<bool> = const { Cell::new(false) };
}

/// Runs `hook` after this thread has staged its next replacement and before it is published.
///
/// Kept thread-local so concurrent tests can force an interleaving without becoming an
/// interleaving themselves. Production builds have no hook.
#[cfg(test)]
pub(crate) fn before_next_publication(hook: impl FnOnce(&Utf8Path) + 'static) {
    BEFORE_PUBLICATION.with(|next| *next.borrow_mut() = Some(Box::new(hook)));
}

/// Runs `hook` after the next conditional comparison and before its rename.
#[cfg(test)]
fn after_next_comparison(hook: impl FnOnce(&Utf8Path) + 'static) {
    AFTER_COMPARISON.with(|next| *next.borrow_mut() = Some(Box::new(hook)));
}

/// Uses `scratch` for this thread's next staged write.
///
/// This is an error-path seam: a directory at that path makes exclusive creation fail without
/// relying on permissions or free disk space.
#[cfg(test)]
pub(crate) fn next_scratch_path(scratch: Utf8PathBuf) {
    TEST_SCRATCH.with(|next| *next.borrow_mut() = Some(scratch));
}

/// Makes the next post-rename directory sync fail on this test thread.
///
/// The rename still completes, which lets command tests exercise the distinction between a
/// published generation and a durability failure without depending on a filesystem fault.
#[cfg(test)]
pub(crate) fn fail_next_directory_sync() {
    DIRECTORY_SYNC_FAILURE.with(|next| next.set(true));
}

#[cfg(test)]
fn take_directory_sync_failure() -> bool {
    DIRECTORY_SYNC_FAILURE.with(|next| next.replace(false))
}

#[cfg(test)]
fn before_publication(path: &Utf8Path) {
    let hook = BEFORE_PUBLICATION.with(|next| next.borrow_mut().take());

    if let Some(hook) = hook {
        hook(path);
    }
}

#[cfg(not(test))]
const fn before_publication(_path: &Utf8Path) {}

#[cfg(test)]
fn after_comparison(path: &Utf8Path) {
    let hook = AFTER_COMPARISON.with(|next| next.borrow_mut().take());

    if let Some(hook) = hook {
        hook(path);
    }
}

#[cfg(not(test))]
const fn after_comparison(_path: &Utf8Path) {}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::process::Command;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    use super::*;

    const EDITOR_PATH: &str = "CARGO_GAMMA_PUBLICATION_EDITOR_PATH";

    fn edit_in_child(path: &Utf8Path) {
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "elements::publication::tests::conditional_publication_has_a_deterministic_external_editor_boundary",
                "--nocapture",
            ])
            .env(EDITOR_PATH, path)
            .status()
            .expect("external editor process");

        assert!(status.success(), "{status}");
    }

    #[test]
    fn writing_a_report_creates_parent_directories() {
        let directory = crate::testing::workdir("elements-write");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let parent = root.join("nested").join("deeper");
        let path = parent.join("report.json");

        // Report paths are often nested artifact locations; the caller should not have to create
        // the directory tree separately.
        write(&path, "{}").expect("write report");

        assert!(parent.is_dir(), "the parent directory was not created");
        assert_eq!(fs::read_to_string(path.as_std_path()).expect("read report"), "{}");
    }

    /// The streaming writer publishes exactly the bytes its filler produced, so a report streamed
    /// through it reaches its name whole rather than in the pieces the filler wrote it in.
    #[test]
    fn a_streamed_write_publishes_the_streamed_bytes_whole() {
        let directory = crate::testing::workdir("elements-stream-ok-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let path = root.join("nested").join("report.json");

        write_streamed(&path, |writer| {
            writer.write_all(b"{\"schemaVersion\":")?;
            writer.write_all(b"\"2\"}")
        })
        .expect("stream report");

        assert_eq!(
            fs::read_to_string(path.as_std_path()).expect("published bytes"),
            "{\"schemaVersion\":\"2\"}"
        );
    }

    /// A filler that fails part-way is the streaming form of a staging failure: the previous file is
    /// left byte for byte, no partial document is published under its name, and the staging sibling
    /// is cleaned up. This is the atomic contract [`write`] upholds, reached through the writer form.
    #[test]
    fn a_streamed_write_that_fails_midway_leaves_the_previous_file_and_no_staging_litter() {
        let directory = crate::testing::workdir("elements-stream-fail-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("the scratch path is UTF-8");
        let path = root.join("report.json");

        fs::write(path.as_std_path(), "the original").expect("seed the destination");

        let error = write_streamed(&path, |writer| {
            writer.write_all(b"half a document")?;

            Err(io::Error::other("the filler gave up"))
        })
        .expect_err("a filler failure must surface");

        assert!(error.to_string().contains("could not write"), "{error}");
        assert!(error.to_string().contains("the filler gave up"), "{error}");
        assert_eq!(fs::read_to_string(path.as_std_path()).expect("previous bytes"), "the original");

        let entries: Vec<String> = fs::read_dir(root.as_std_path())
            .expect("read the directory")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();

        assert_eq!(entries, ["report.json"], "the staging sibling must be cleaned up: {entries:?}");
    }

    /// A failed directory sync is not a failed publication: the bytes already reached their final
    /// name. Callers need that distinction to register source rollback before returning the
    /// durability error.
    #[test]
    fn a_conditional_replacement_reports_post_rename_sync_failure_as_published() {
        let directory = crate::testing::workdir("elements-conditional-sync-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf-8");
        let path = root.join("source.rs");

        fs::write(path.as_std_path(), "before").expect("source");
        fail_next_directory_sync();

        let publication = write_if_unchanged(&root, &path, Some("before"), "after").expect("the rename itself succeeds");

        assert!(matches!(publication, Publication::PublishedUndurable(_)), "{publication:?}");
        assert_eq!(fs::read_to_string(path.as_std_path()).expect("published bytes"), "after");
        assert!(
            !root.join(".source.rs.cargo-gamma.lock").exists(),
            "publication locks belong in the external workspace cache"
        );
    }

    /// Removing the final name has the same post-publication durability boundary as replacing it.
    #[test]
    fn a_conditional_removal_reports_post_unlink_sync_failure_as_published() {
        let directory = crate::testing::workdir("elements-conditional-remove-sync-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf-8");
        let path = root.join("source.rs");

        fs::write(path.as_std_path(), "before").expect("source");
        fail_next_directory_sync();

        let publication = remove_if_unchanged(&root, &path, "before").expect("the removal itself succeeds");

        assert!(matches!(publication, Publication::PublishedUndurable(_)), "{publication:?}");
        assert!(!path.exists(), "the name was removed before the directory sync failed");
    }

    #[test]
    fn conditional_publication_has_a_deterministic_external_editor_boundary() {
        if let Some(path) = std::env::var_os(EDITOR_PATH) {
            fs::write(path, "editor").expect("external editor writes");
            return;
        }

        let directory = crate::testing::workdir("elements-external-editor-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf-8");
        let path = root.join("source.rs");

        fs::write(path.as_std_path(), "before").expect("source");
        let edited = path.clone();
        before_next_publication(move |_| edit_in_child(&edited));

        let conflict = write_if_unchanged(&root, &path, Some("before"), "gamma").expect("comparison");

        assert!(matches!(conflict, Publication::Conflict), "{conflict:?}");
        assert_eq!(fs::read_to_string(&path).expect("editor bytes"), "editor");

        fs::write(path.as_std_path(), "before").expect("reset source");
        let edited = path.clone();
        after_next_comparison(move |_| edit_in_child(&edited));

        let published = write_if_unchanged(&root, &path, Some("before"), "gamma").expect("publication");

        assert!(matches!(published, Publication::Published), "{published:?}");
        assert_eq!(
            fs::read_to_string(&path).expect("published bytes"),
            "gamma",
            "the API must not claim to preserve non-cooperating edits made after its comparison"
        );
    }

    #[test]
    fn a_publish_sync_failure_removes_its_staging_file() {
        let directory = crate::testing::workdir("elements-publish-sync-failure-");
        let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).expect("utf-8");
        let path = root.join("gamma.toml");

        fail_next_directory_sync();

        let error = publish(&path, "jobs = 2\n").expect_err("directory sync must fail");
        let entries: Vec<_> = fs::read_dir(&root)
            .expect("directory")
            .map(|entry| entry.expect("entry").file_name())
            .collect();

        assert!(error.to_string().contains("injected directory sync failure"), "{error}");
        assert_eq!(fs::read_to_string(&path).expect("published destination"), "jobs = 2\n");
        assert_eq!(entries, vec![path.file_name().expect("name")]);
    }

    /// The visible file changes from one whole version to the next, with nothing partial in
    /// between and nothing left beside it.
    ///
    /// A CI job parsing the JSON report cannot tell a truncated file from a short one, so a write
    /// interrupted part-way has to leave the previous report rather than a broken one. Staging in
    /// a sibling and renaming is what buys that; this asserts the observable half of it — the
    /// destination is never the staging file, and the staging file does not survive the write.
    #[test]
    fn a_report_is_written_whole_rather_than_streamed_into_its_destination() {
        let dir = tempfile::TempDir::new().expect("temp");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let path = root.join("nested").join("report.json");

        write(&path, "{\"first\":true}").expect("first write");
        write(&path, "{\"second\":true}").expect("second write");

        assert_eq!(fs::read_to_string(path.as_std_path()).expect("read"), "{\"second\":true}");

        let leftovers: Vec<String> = fs::read_dir(path.parent().expect("parent").as_std_path())
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();

        assert_eq!(leftovers, vec!["report.json".to_owned()], "the staging file must not survive");
    }

    /// A write that cannot be completed leaves the previous file exactly as it was.
    ///
    /// The rename is the only step that can be observed, so making it fail — by pointing it at a
    /// directory, which no platform will replace with a file — is what exercises the failure path
    /// that the old truncate-then-stream writer had no answer for.
    #[test]
    fn a_write_that_cannot_be_completed_leaves_the_previous_file_alone() {
        let dir = tempfile::TempDir::new().expect("temp");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let path = root.join("report.json");

        fs::create_dir(path.as_std_path()).expect("a destination the rename cannot replace");

        assert!(write(&path, "{}").is_err(), "the failure must be reported rather than swallowed");
        assert!(path.as_std_path().is_dir(), "the destination must be untouched");

        let leftovers = fs::read_dir(root.as_std_path()).expect("read dir").count();

        assert_eq!(leftovers, 1, "a failed rename must not leave its staging file behind");
    }

    #[test]
    fn a_path_with_no_parent_at_all_skips_directory_creation() {
        // An empty path has nothing above it to create, and asking `create_dir_all` to create
        // nothing would either do nothing useful or fail for a reason that has nothing to do with
        // the report itself. Skipping straight to the write is what keeps that irrelevant failure
        // out of the caller's way.
        let path = Utf8PathBuf::new();

        assert!(path.parent().is_none());
        assert!(write(&path, "{}").is_err());
    }

    /// A write that fails before it publishes leaves the previous contents byte for byte.
    ///
    /// Staging is the only step that can be made to fail from a test without a full disk, and
    /// blocking it stands in for every way the bytes can fail to arrive — which is the case the old
    /// truncate-then-stream writer answered by destroying the file first and hoping.
    #[test]
    fn a_write_that_cannot_be_staged_leaves_the_previous_contents_alone() {
        let dir = crate::testing::workdir("elements-staging-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let path = root.join("report.json");

        fs::write(path.as_std_path(), "{\"original\":true}").expect("the original");
        let scratch = root.join(".blocked-stage");
        fs::create_dir(scratch.as_std_path()).expect("something the staging file cannot be");
        next_scratch_path(scratch);

        let error = write(&path, "{\"replacement\":true}").expect_err("the write must fail");

        assert!(error.to_string().contains("report.json"), "{error}");
        assert_eq!(
            fs::read_to_string(path.as_std_path()).expect("read"),
            "{\"original\":true}",
            "the previous contents were not left alone"
        );
    }

    /// A file this tool rewrites is the user's, and so are its permissions: a mode of 0o600 on a
    /// source file is a decision, and editing the file is no reason to hand it back as 0o644.
    #[cfg(unix)]
    #[test]
    fn a_replaced_file_keeps_the_permissions_it_had() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::testing::workdir("elements-permissions-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let path = root.join("source.rs");

        fs::write(path.as_std_path(), "fn f() {}\n").expect("the original");
        fs::set_permissions(path.as_std_path(), fs::Permissions::from_mode(0o640)).expect("mode");

        write(&path, "fn g() {}\n").expect("write");

        let mode = fs::metadata(path.as_std_path()).expect("metadata").permissions().mode() & 0o777;

        assert_eq!(mode, 0o640, "the file came back with different permissions");
        assert_eq!(fs::read_to_string(path.as_std_path()).expect("read"), "fn g() {}\n");
    }

    /// Replacing the contents of a linked file must replace what the link points at. Renaming onto
    /// the link itself would delete the link and leave an unrelated file in its place — and if the
    /// link crosses a filesystem, the rename would not even be permitted.
    #[cfg(unix)]
    #[test]
    fn a_write_through_a_symlink_replaces_what_it_points_at() {
        let dir = crate::testing::workdir("elements-symlink-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let target = root.join("real.rs");
        let link = root.join("link.rs");

        fs::write(target.as_std_path(), "fn f() {}\n").expect("the original");
        std::os::unix::fs::symlink(target.as_std_path(), link.as_std_path()).expect("symlink");

        write(&link, "fn g() {}\n").expect("write");

        assert!(
            fs::symlink_metadata(link.as_std_path()).expect("metadata").file_type().is_symlink(),
            "the link was replaced by a file"
        );
        assert_eq!(fs::read_to_string(target.as_std_path()).expect("read"), "fn g() {}\n");
    }

    /// A dangling link still names the destination that a write must create. Falling back to the
    /// unresolved link name would replace the link itself during the final rename.
    #[cfg(unix)]
    #[test]
    fn a_write_through_a_dangling_symlink_preserves_the_link_and_creates_its_target() {
        let dir = crate::testing::workdir("elements-dangling-symlink-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let target = root.join("created").join("real.rs");
        let link = root.join("link.rs");

        std::os::unix::fs::symlink("created/real.rs", link.as_std_path()).expect("symlink");

        write(&link, "fn g() {}\n").expect("write");

        assert!(
            fs::symlink_metadata(link.as_std_path()).expect("metadata").file_type().is_symlink(),
            "the dangling link was replaced"
        );
        assert_eq!(fs::read_to_string(target.as_std_path()).expect("target"), "fn g() {}\n");
    }

    /// Publishing takes a free name and reports, rather than takes, a name that is not free.
    #[test]
    fn publishing_takes_a_free_name_and_leaves_a_taken_one_alone() {
        let dir = crate::testing::workdir("elements-publish-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let path = root.join("nested").join("gamma.toml");

        assert!(publish(&path, "jobs = 2\n").expect("publish"), "a free name must be taken");
        assert_eq!(fs::read_to_string(path.as_std_path()).expect("read"), "jobs = 2\n");

        assert!(
            !publish(&path, "jobs = 9\n").expect("publish"),
            "a name that is taken must be reported, not overwritten"
        );
        assert_eq!(
            fs::read_to_string(path.as_std_path()).expect("read"),
            "jobs = 2\n",
            "the file that was already there was overwritten"
        );

        let leftovers: Vec<String> = fs::read_dir(path.parent().expect("parent").as_std_path())
            .expect("read dir")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();

        assert_eq!(leftovers, vec!["gamma.toml".to_owned()], "the staging file must not survive");
    }

    /// A publish that fails leaves nothing at all under the final name.
    ///
    /// This is the whole reason the bytes are staged first. A file created under the final name and
    /// then written into would, on a failure, leave a corrupt file that the retry refuses to
    /// overwrite — the migration would be permanently stuck behind its own wreckage.
    #[test]
    fn a_publish_that_cannot_be_staged_leaves_no_file_to_block_the_retry() {
        let dir = crate::testing::workdir("elements-publish-failure-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let path = root.join("gamma.toml");
        let scratch = root.join(".blocked-stage");

        fs::create_dir(scratch.as_std_path()).expect("something the staging file cannot be");
        next_scratch_path(scratch.clone());

        let error = publish(&path, "jobs = 2\n").expect_err("the publish must fail");

        assert!(error.to_string().contains("gamma.toml"), "{error}");
        assert!(!path.as_std_path().exists(), "a failed publish left the final name taken");

        // And once whatever blocked it is gone, the same call succeeds: nothing about the failure
        // is sticky.
        fs::remove_dir(scratch.as_std_path()).expect("clear the obstruction");
        assert!(publish(&path, "jobs = 2\n").expect("retry"));
        assert_eq!(fs::read_to_string(path.as_std_path()).expect("read"), "jobs = 2\n");
    }

    /// A staging file that cannot be removed is named, because nothing else will ever mention it.
    #[test]
    fn a_staging_file_that_cannot_be_removed_is_reported_with_the_failure() {
        let dir = crate::testing::workdir("elements-discard-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let path = root.join("report.json");
        let scratch = root.join(".unremovable-stage");

        // A directory cannot be removed by `remove_file`, so this stands in for a staging file the
        // process cannot clean up after itself.
        fs::create_dir(scratch.as_std_path()).expect("an unremovable staging path");
        next_scratch_path(scratch.clone());

        let error = write(&path, "{}").expect_err("the write must fail");

        assert!(error.to_string().contains("could not be removed"), "{error}");
        assert!(error.to_string().contains(scratch.file_name().expect("name")), "{error}");
    }

    /// A staging name that is already taken is refused rather than written through.
    ///
    /// The name is a sibling of the destination and is derived rather than negotiated, so the only
    /// thing standing between two writers of one destination is that the name distinguishes them.
    /// If it ever does not — two pid namespaces over one bind mount, or a name planted in a
    /// world-writable directory — truncating into it publishes one writer's bytes over the other's
    /// under a name a CI job then parses. Refusing costs a failed write; taking it over costs a
    /// report that is not valid JSON.
    #[test]
    fn a_staging_name_that_is_already_taken_is_refused_rather_than_written_through() {
        let dir = crate::testing::workdir("elements-exclusive-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let path = root.join("report.json");

        fs::write(path.as_std_path(), "{\"original\":true}").expect("the original");
        let scratch = root.join(".taken-stage");
        fs::write(scratch.as_std_path(), "somebody else's staging file").expect("the staging file");
        next_scratch_path(scratch);

        let error = write(&path, "{\"replacement\":true}").expect_err("the write must fail");

        assert!(error.to_string().contains("report.json"), "{error}");
        assert_eq!(
            fs::read_to_string(path.as_std_path()).expect("read"),
            "{\"original\":true}",
            "the destination was published from a staging file this call did not create"
        );
    }

    /// The staging name carries entropy of its own, not only the process id.
    ///
    /// A pid is unique within a pid namespace; the name has to be unique on a filesystem, and two
    /// containers sharing a bind-mounted workspace both start at pid 1.
    #[test]
    fn a_staging_name_is_not_derived_from_the_process_id_alone() {
        let path = Utf8Path::new("/w/report.json");
        let first = scratch_path(path).file_name().expect("a name").to_owned();
        let second = scratch_path(path).file_name().expect("a name").to_owned();

        assert_ne!(first, second, "two invocations must not share a staging path");
        assert_ne!(first, format!(".report.json.{}.tmp", std::process::id()));
        assert!(first.contains(&std::process::id().to_string()), "{first}");
        assert!(second.contains(&std::process::id().to_string()), "{second}");
    }

    /// Three writers stop after staging, so every staging path is simultaneously live. A shared
    /// path would make the second writer remove the first's staged bytes on its error path; unique
    /// names let all three publish only bytes they staged themselves.
    #[test]
    fn concurrent_writers_neither_remove_nor_publish_another_writers_staging_file() {
        let dir = crate::testing::workdir("elements-concurrent-staging-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let path = root.join("report.json");
        let barrier = Arc::new(Barrier::new(3));
        let staged = Arc::new(Mutex::new(Vec::new()));
        let mut writers = Vec::new();

        for contents in ["first", "second", "third"] {
            let barrier = Arc::clone(&barrier);
            let staged = Arc::clone(&staged);
            let path = path.clone();

            writers.push(thread::spawn(move || {
                before_next_publication(move |scratch| {
                    assert_eq!(fs::read_to_string(scratch).expect("staged bytes"), contents);
                    staged.lock().expect("staged paths").push(scratch.to_path_buf());
                    let _ = barrier.wait();
                });

                write(&path, contents)
            }));
        }

        for writer in writers {
            writer.join().expect("writer panicked").expect("write");
        }

        let mut staged = staged.lock().expect("staged paths").clone();

        staged.sort();
        staged.dedup();

        assert_eq!(staged.len(), 3, "every invocation must own its staging path");
        assert!(["first", "second", "third"].contains(&fs::read_to_string(path).expect("published bytes").as_str()));
    }

    /// The rename is only as durable as the directory that records it.
    ///
    /// The crash itself cannot be staged from a test, so what is pinned here is that the parent is
    /// opened and synced at all, and that a parent it cannot open is reported rather than passed
    /// off as a completed write.
    #[cfg(unix)]
    #[test]
    fn the_directory_a_published_name_lives_in_is_synced_too() {
        let dir = crate::testing::workdir("elements-durable-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let path = root.join("report.json");

        write(&path, "{}").expect("write");
        published(&path).expect("the parent of a written file is syncable");

        assert!(published(&root.join("absent").join("report.json")).is_err());

        // A bare file name has a parent of `""`, which names no directory at all: the process's own
        // directory is what its rename touched.
        published(Utf8Path::new("report.json")).expect("a relative name syncs the working directory");
    }

    /// A file this tool rewrites keeps the permissions it has when the contents are finished.
    ///
    /// The mode is read as late as it can be, so that a change made during the write is carried
    /// rather than reverted. It is applied after the bytes rather than before them, which on Unix
    /// costs a microsecond of exposure under the default creation mode and on Windows is the
    /// difference between a read-only marking that the open handle predates and one it does not.
    #[cfg(unix)]
    #[test]
    fn a_read_only_destination_is_replaced_and_stays_read_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::testing::workdir("elements-read-only-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8");
        let path = root.join("report.json");

        fs::write(path.as_std_path(), "{\"original\":true}").expect("the original");
        fs::set_permissions(path.as_std_path(), fs::Permissions::from_mode(0o444)).expect("mode");

        write(&path, "{\"replacement\":true}").expect("write");

        let mode = fs::metadata(path.as_std_path()).expect("metadata").permissions().mode() & 0o777;

        assert_eq!(mode, 0o444, "the file came back with different permissions");
        assert_eq!(fs::read_to_string(path.as_std_path()).expect("read"), "{\"replacement\":true}");
    }
}
