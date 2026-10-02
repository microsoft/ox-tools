// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs::{File, OpenOptions};
use std::path::Path;

use ohno::IntoAppError;
use tokio::task::JoinHandle;

use crate::Result;

/// Log target for `cache_lock`
const LOG_TARGET: &str = " collector";

/// Guard that releases the cache lock when dropped
#[derive(Debug)]
pub struct CacheLockGuard(File);

async fn finish_lock_task(task: JoinHandle<Result<File>>) -> Result<File> {
    task.await.into_app_err("lock task panicked")?
}

impl Drop for CacheLockGuard {
    // Releasing an advisory lock on a file handle we own cannot fail short of the
    // operating system misbehaving, so the failure arm is unreachable in a test.
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[mutants::skip] // Dropping the owned file closes it and releases the lock even if this explicit unlock is removed.
    // #[gamma::skip(fn_value.unit, tag = "equivalent", reason = "dropping the owned file handle releases the advisory lock even when the explicit unlock body is replaced with unit")]
    fn drop(&mut self) {
        // Lock is automatically released when the file is closed
        // Log if unlock fails (shouldn't happen in normal operation)
        if let Err(e) = self.0.unlock() {
            log::warn!(target: LOG_TARGET, "Could not unlock cache: {e:#}");
        }
    }
}

fn lock_file_with(file: File, lock_path: &Path, lock: impl FnOnce(&File) -> std::io::Result<()>) -> Result<File> {
    lock(&file).into_app_err_with(|| format!("acquiring exclusive lock on cache at '{}'", lock_path.display()))?;
    log::debug!(target: LOG_TARGET, "Acquired cache lock at '{}'", lock_path.display());
    Ok(file)
}

async fn finish_cache_lock(task: JoinHandle<Result<File>>) -> Result<CacheLockGuard> {
    Ok(CacheLockGuard(finish_lock_task(task).await?))
}

/// Acquire a cache lock using advisory file locking
pub async fn acquire_cache_lock(cache_dir: &Path) -> Result<CacheLockGuard> {
    let lock_path = cache_dir.join("cache.lock");

    // Create or open the lock file
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .into_app_err_with(|| format!("opening cache lock file at '{}'", lock_path.display()))?;

    // Block until we can acquire the lock
    // This needs to run in a blocking task since it may block for an extended time
    let task = tokio::task::spawn_blocking(move || lock_file_with(file, &lock_path, File::lock));

    finish_cache_lock(task).await
}

#[cfg(test)]
#[cfg(not(miri))]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    async fn test_acquire_lock_creates_lock_file() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let lock_path = temp_dir.path().join("cache.lock");

        assert!(!lock_path.exists());

        let guard = acquire_cache_lock(temp_dir.path()).await;
        assert!(guard.is_ok());
        assert!(lock_path.exists());

        drop(guard);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    async fn acquiring_an_existing_lock_does_not_truncate_it() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let lock_path = temp_dir.path().join("cache.lock");
        std::fs::write(&lock_path, b"preserve me").expect("lock fixture is writable");

        let guard = acquire_cache_lock(temp_dir.path())
            .await
            .expect("existing lock file can be acquired");
        drop(guard);

        assert_eq!(std::fs::read(&lock_path).expect("lock fixture is readable"), b"preserve me");
    }

    #[tokio::test]
    async fn a_panicking_lock_task_is_returned_as_a_descriptive_error() {
        let task = tokio::spawn(async { panic!("deliberate lock-task panic") });

        let error = finish_lock_task(task).await.expect_err("a panicking task must not be unwrapped");

        assert!(error.to_string().contains("lock task panicked"), "{error}");
    }

    #[test]
    fn advisory_lock_failures_are_returned_without_panicking() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let lock_path = temp_dir.path().join("cache.lock");
        let file = File::create(&lock_path).expect("lock fixture is writable");

        let error = lock_file_with(file, &lock_path, |_| Err(std::io::Error::other("lock failed")))
            .expect_err("advisory lock failures must be returned");

        assert!(error.to_string().contains("acquiring exclusive lock on cache"), "{error}");
    }

    #[tokio::test]
    async fn cache_lock_construction_returns_join_failures_without_panicking() {
        let task = tokio::spawn(async { panic!("deliberate cache-lock panic") });

        let error = finish_cache_lock(task)
            .await
            .expect_err("a panicking cache-lock task must be returned as an error");

        assert!(error.to_string().contains("lock task panicked"), "{error}");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    async fn test_lock_released_on_drop() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");

        // Acquire and release
        let guard = acquire_cache_lock(temp_dir.path()).await.unwrap();
        drop(guard);

        // Should be able to re-acquire immediately after release
        let guard2 = acquire_cache_lock(temp_dir.path()).await;
        let _ = guard2.unwrap();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    async fn test_acquire_lock_twice_sequentially() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");

        let guard1 = acquire_cache_lock(temp_dir.path()).await.unwrap();
        drop(guard1);

        let guard2 = acquire_cache_lock(temp_dir.path()).await.unwrap();
        drop(guard2);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetFullPathNameW")]
    async fn test_acquire_lock_nonexistent_directory() {
        let path = Path::new("this_directory_does_not_exist_at_all_98765");
        let result = acquire_cache_lock(path).await;
        let _ = result.unwrap_err();
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    async fn test_lock_guard_debug() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let guard = acquire_cache_lock(temp_dir.path()).await.unwrap();
        let debug = format!("{guard:?}");
        assert!(debug.contains("CacheLockGuard"));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore = "Miri cannot call GetTempPathW")]
    async fn test_exclusive_lock_blocks_concurrent_access() {
        use std::sync::Arc;

        use tokio::sync::Barrier;

        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let dir_path = temp_dir.path().to_path_buf();

        let barrier = Arc::new(Barrier::new(2));
        let counter = Arc::new(core::sync::atomic::AtomicU32::new(0));

        // Task 1: acquire lock, wait for task 2 to start, hold lock briefly
        let b1 = Arc::clone(&barrier);
        let c1 = Arc::clone(&counter);
        let d1 = dir_path.clone();
        let t1 = tokio::spawn(async move {
            let guard = acquire_cache_lock(&d1).await.unwrap();
            let _ = c1.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            let _ = b1.wait().await;
            // Hold lock for a bit so task 2 must wait
            tokio::time::sleep(core::time::Duration::from_millis(50)).await;
            drop(guard);
        });

        // Task 2: wait until task 1 has the lock, then try to acquire
        let b2 = Arc::clone(&barrier);
        let c2 = Arc::clone(&counter);
        let t2 = tokio::spawn(async move {
            let _ = b2.wait().await;
            // Task 1 already holds the lock
            let guard = acquire_cache_lock(&dir_path).await.unwrap();
            // By the time we get here, task 1 should have incremented
            assert!(c2.load(core::sync::atomic::Ordering::SeqCst) >= 1);
            drop(guard);
        });

        t1.await.unwrap();
        t2.await.unwrap();
    }
}
