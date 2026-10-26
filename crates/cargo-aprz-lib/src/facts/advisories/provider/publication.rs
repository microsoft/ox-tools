// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::time::Duration;
use std::path::{Path, PathBuf};

use ohno::IntoAppError;

use super::{LOG_TARGET, archive_identity, snapshot_path};
use crate::Result;
use crate::facts::cache::{Cache, CacheResult};

/// External I/O for publishing one validated staging directory.
#[cfg_attr(test, fakeable::fakeable(fake_impl = tests::MemoryPublication, fakes_feature = false))]
pub(crate) struct SnapshotIo {
    cache: Cache,
    stage: Option<tempfile::TempDir>,
}

#[cfg_attr(test, fakeable::fakeable(fakes_feature = false))]
impl SnapshotIo {
    pub(crate) fn new(cache: Cache, stage: tempfile::TempDir) -> Self {
        Self { cache, stage: Some(stage) }
    }

    pub(crate) fn previous_snapshot(&self) -> Result<Option<PathBuf>> {
        match Cache::new(self.cache.dir(), Duration::MAX, false).load::<(String, String)>("last_synced.bin") {
            CacheResult::Data((_, directory)) if directory.starts_with(".advisories-") => {
                Ok(Some(snapshot_path(self.cache.dir(), &directory)?))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn publish_marker(&self, url: &str) -> Result<()> {
        let stage = self
            .stage
            .as_ref()
            .expect("publication retains the stage only after publishing its marker");
        let directory = stage
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ohno::app_err!("advisory snapshot directory name is not UTF-8"))?;
        let marker = tempfile::NamedTempFile::new_in(self.cache.dir())?;
        let filename = marker
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ohno::app_err!("advisory marker filename is not UTF-8"))?;
        self.cache.save(filename, &(archive_identity(url), directory))?;
        marker
            .persist(self.cache.dir().join("last_synced.bin"))
            .map_err(|error| error.error)
            .into_app_err("publishing the advisory cache synchronization marker")?;
        Ok(())
    }

    pub(crate) fn retain_snapshot(&mut self) -> PathBuf {
        self.stage.take().expect("publication retains its stage exactly once").keep()
    }

    pub(crate) fn root(&self) -> &Path {
        self.cache.dir()
    }

    #[expect(clippy::unused_self, reason = "the receiver selects the in-memory I/O adapter in tests")]
    pub(crate) fn remove_snapshot(&self, path: &Path) -> std::io::Result<()> {
        std::fs::remove_dir_all(path)
    }
}

/// Replacing the marker is the commit point. Until then the I/O adapter owns
/// the stage and removes it on failure; subsequent cleanup is best-effort.
pub(super) fn publish_snapshot(mut io: SnapshotIo, url: &str) -> Result<()> {
    let previous = io.previous_snapshot()?;
    io.publish_marker(url)?;
    let published = io.retain_snapshot();
    if let Some(previous) = previous
        && previous != published
        && let Err(error) = io.remove_snapshot(&previous)
    {
        log::warn!(target: LOG_TARGET, "Could not remove old advisory snapshot '{}': {error}", previous.display());
    }
    let legacy = io.root().join("repo");
    if let Err(error) = io.remove_snapshot(&legacy)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!(target: LOG_TARGET, "Could not remove old advisory snapshot '{}': {error}", legacy.display());
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex, mpsc};

    use super::*;
    use crate::facts::advisories::provider::with_cache_lock;

    const OLD: &str = ".advisories-old";
    const NEW: &str = ".advisories-new";

    #[derive(Debug)]
    struct MemoryState {
        locked: bool,
        snapshots: BTreeSet<PathBuf>,
        marker: (String, String),
        removals: Vec<PathBuf>,
    }

    impl MemoryState {
        fn seeded() -> Arc<Mutex<Self>> {
            Arc::new(Mutex::new(Self {
                locked: false,
                snapshots: [OLD, NEW, "repo"].map(|name| Path::new("cache").join(name)).into(),
                marker: (archive_identity("old-url"), OLD.to_owned()),
                removals: Vec::new(),
            }))
        }

        fn try_lock(state: &Arc<Mutex<Self>>) -> Result<Arc<MemoryLease>, std::fs::TryLockError> {
            let mut inner = state.lock().expect("no test panics while holding the state lock");
            if inner.locked {
                return Err(std::fs::TryLockError::WouldBlock);
            }
            inner.locked = true;
            Ok(Arc::new(MemoryLease(Arc::clone(state))))
        }
    }

    /// The lock is released only when the final owner of this lease drops.
    #[derive(Debug)]
    struct MemoryLease(Arc<Mutex<MemoryState>>);

    impl Drop for MemoryLease {
        fn drop(&mut self) {
            self.0.lock().expect("no test panics while holding the state lock").locked = false;
        }
    }

    struct Checkpoint {
        started: tokio::sync::oneshot::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    pub(crate) struct MemoryPublication {
        root: PathBuf,
        state: Arc<Mutex<MemoryState>>,
        checkpoint: Mutex<Option<Checkpoint>>,
        fail_marker: bool,
        fail_cleanup: bool,
        retained: bool,
    }

    impl MemoryPublication {
        fn new(state: Arc<Mutex<MemoryState>>) -> Self {
            Self {
                root: "cache".into(),
                state,
                checkpoint: Mutex::new(None),
                fail_marker: false,
                fail_cleanup: false,
                retained: false,
            }
        }

        pub(crate) fn previous_snapshot(&self) -> Result<Option<PathBuf>> {
            let state = self.state.lock().expect("no test panics while holding the state lock");
            snapshot_path(&self.root, &state.marker.1).map(Some)
        }

        pub(crate) fn publish_marker(&self, url: &str) -> Result<()> {
            let checkpoint = self
                .checkpoint
                .lock()
                .expect("no test panics while holding the checkpoint lock")
                .take();
            if let Some(checkpoint) = checkpoint {
                let _ = checkpoint.started.send(());
                // Dropping the sender during unwinding also releases the worker.
                let _ = checkpoint.release.recv();
            }
            if self.fail_marker {
                return Err(ohno::app_err!("scripted marker publication failure"));
            }
            self.state.lock().expect("no test panics while holding the state lock").marker = (archive_identity(url), NEW.to_owned());
            Ok(())
        }

        pub(crate) fn retain_snapshot(&mut self) -> PathBuf {
            self.retained = true;
            self.root.join(NEW)
        }

        pub(crate) fn root(&self) -> &Path {
            &self.root
        }

        pub(crate) fn remove_snapshot(&self, path: &Path) -> std::io::Result<()> {
            let mut state = self.state.lock().expect("no test panics while holding the state lock");
            state.removals.push(path.to_path_buf());
            if self.fail_cleanup {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
            if !state.snapshots.remove(path) {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
            }
            Ok(())
        }
    }

    impl Drop for MemoryPublication {
        fn drop(&mut self) {
            if !self.retained {
                self.state
                    .lock()
                    .expect("no test panics while holding the state lock")
                    .snapshots
                    .remove(&self.root.join(NEW));
            }
        }
    }

    #[tokio::test]
    async fn cancelled_cache_publication_keeps_the_lock_until_blocking_work_finishes() {
        let state = MemoryState::seeded();
        let lease = MemoryState::try_lock(&state).expect("the first owner acquires the memory lease");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let mut io = MemoryPublication::new(Arc::clone(&state));
        io.checkpoint = Mutex::new(Some(Checkpoint {
            started: started_tx,
            release: release_rx,
        }));
        let caller = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let result = with_cache_lock(lease, || publish_snapshot(SnapshotIo::fake(io), "new-url"));
                let _ = finished_tx.send(result);
            })
            .await
        });

        started_rx
            .await
            .expect("the worker reaches marker publication while holding its lease");
        caller.abort();
        let cancelled = caller.await;
        let contender = MemoryState::try_lock(&state);
        // Release and observe the complete coordinator (including lease destruction)
        // before asserting, so even a regression cannot strand a blocking worker.
        let released = release_tx.send(());
        let finished = finished_rx.await;

        released.expect("the blocked publication worker still owns its receiver");
        finished
            .expect("the worker reports completion")
            .expect("detached publication succeeds");
        assert!(
            cancelled
                .expect_err("the caller was cancelled while its worker was blocked")
                .is_cancelled()
        );
        assert!(matches!(contender, Err(std::fs::TryLockError::WouldBlock)));
        let _next_lease = MemoryState::try_lock(&state).expect("completion includes releasing the worker's lease");
        let state = state.lock().expect("no test panics while holding the state lock");
        assert_eq!(state.marker, (archive_identity("new-url"), NEW.to_owned()));
        assert_eq!(state.snapshots, [Path::new("cache").join(NEW)].into());
        assert_eq!(state.removals, [OLD, "repo"].map(|name| Path::new("cache").join(name)));
    }

    #[test]
    fn marker_publication_failure_preserves_previous_snapshot_and_releases_lease() {
        let state = MemoryState::seeded();
        let lease = MemoryState::try_lock(&state).expect("the first owner acquires the memory lease");
        let mut io = MemoryPublication::new(Arc::clone(&state));
        io.fail_marker = true;

        let error =
            with_cache_lock(lease, || publish_snapshot(SnapshotIo::fake(io), "new-url")).expect_err("the scripted marker write must fail");

        assert_eq!(error.to_string(), "scripted marker publication failure");
        let _next_lease = MemoryState::try_lock(&state).expect("failed publication releases the lease");
        let state = state.lock().expect("no test panics while holding the state lock");
        assert_eq!(state.marker, (archive_identity("old-url"), OLD.to_owned()));
        assert_eq!(state.snapshots, [OLD, "repo"].map(|name| Path::new("cache").join(name)).into());
        assert!(state.removals.is_empty());
    }

    #[test]
    fn cleanup_failure_does_not_undo_publication_or_retain_lease() {
        let state = MemoryState::seeded();
        let lease = MemoryState::try_lock(&state).expect("the first owner acquires the memory lease");
        let mut io = MemoryPublication::new(Arc::clone(&state));
        io.fail_cleanup = true;

        with_cache_lock(lease, || publish_snapshot(SnapshotIo::fake(io), "new-url")).expect("cleanup is best-effort");

        let _next_lease = MemoryState::try_lock(&state).expect("cleanup failure releases the lease");
        let state = state.lock().expect("no test panics while holding the state lock");
        assert_eq!(state.marker, (archive_identity("new-url"), NEW.to_owned()));
        assert_eq!(state.snapshots, [OLD, NEW, "repo"].map(|name| Path::new("cache").join(name)).into());
        assert_eq!(state.removals, [OLD, "repo"].map(|name| Path::new("cache").join(name)));
    }
}
