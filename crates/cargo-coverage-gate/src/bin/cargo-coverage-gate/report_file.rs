// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native staging and atomic publication of coverage artifacts.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, io};

use ohno::{AppError, IntoAppError};

pub(crate) static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn reserve_temporary_sequence(sequence: &AtomicU64) -> u64 {
    sequence.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn missing_lcov_parent_error(path: &Path) -> AppError {
    AppError::new(format!(
        "LCOV path `{}` has no parent directory",
        crate::package_glob::diagnostic(&path.display().to_string())
    ))
}

pub(crate) fn publish_reports<'a>(reports: impl IntoIterator<Item = (&'a Path, String, &'a str)>) -> Result<(), AppError> {
    let reports = reports.into_iter();
    let mut staged = Vec::with_capacity(reports.size_hint().0);
    for (target, label, contents) in reports {
        let directory = target.parent().ok_or_else(|| missing_lcov_parent_error(target))?;
        let temporary = TemporaryPath::write(directory, &label, contents.as_bytes())?;
        staged.push((temporary, target));
    }
    for (temporary, target) in staged {
        temporary.publish(target)?;
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct TemporaryPath {
    pub(crate) path: PathBuf,
    pub(crate) armed: bool,
}

/// A temporary-file writer boundary that makes durability failures independently testable.
pub(crate) trait SynchronizedWrite: io::Write {
    fn sync_all(&self) -> io::Result<()>;
}

impl SynchronizedWrite for fs::File {
    #[mutants::skip] // Trivial File delegation; the injected writer test double covers synchronization failures.
    fn sync_all(&self) -> io::Result<()> {
        Self::sync_all(self)
    }
}

impl TemporaryPath {
    pub(crate) fn new(directory: &Path, label: &str) -> Self {
        let sequence = reserve_temporary_sequence(&TEMPORARY_SEQUENCE);
        Self {
            path: directory.join(format!(".coverage-gate-{}-{sequence}-{label}", std::process::id())),
            armed: false,
        }
    }

    pub(crate) fn write(directory: &Path, label: &str, contents: &[u8]) -> Result<Self, AppError> {
        let (temporary, mut file) = Self::create(directory, label)?;
        Self::finish_write(temporary, &mut file, contents)
    }

    pub(crate) fn finish_write(temporary: Self, writer: &mut impl SynchronizedWrite, contents: &[u8]) -> Result<Self, AppError> {
        write_temporary_contents(&temporary, writer, contents)?;
        Ok(temporary)
    }

    pub(crate) fn create(directory: &Path, label: &str) -> Result<(Self, fs::File), AppError> {
        let mut temporary = Self::new(directory, label);
        let file = temporary.create_with(|path| fs::OpenOptions::new().write(true).create_new(true).open(path))?;
        Ok((temporary, file))
    }

    pub(crate) fn create_with<T>(&mut self, open: impl FnOnce(&Path) -> io::Result<T>) -> Result<T, AppError> {
        let file = open(self.path()).into_app_err(format!("failed to create temporary file `{}`", self.path().display()))?;
        self.armed = true;
        Ok(file)
    }

    pub(crate) fn cleanup_with(&mut self, remove: impl FnOnce(&Path) -> io::Result<()>) {
        if std::mem::take(&mut self.armed) {
            let _ = remove(&self.path);
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn publish(self, target: &Path) -> Result<(), AppError> {
        fs::rename(&self.path, target).into_app_err(format!(
            "failed to publish staged coverage report `{}` to `{}`",
            self.path.display(),
            target.display()
        ))?;
        Ok(())
    }
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        self.cleanup_with(|path| fs::remove_file(path));
    }
}

pub(crate) fn write_temporary_contents(
    temporary: &TemporaryPath,
    writer: &mut impl SynchronizedWrite,
    contents: &[u8],
) -> Result<(), AppError> {
    writer
        .write_all(contents)
        .into_app_err(format!("failed to write temporary file `{}`", temporary.path().display()))?;
    writer
        .sync_all()
        .into_app_err(format!("failed to flush temporary file `{}`", temporary.path().display()))
}
