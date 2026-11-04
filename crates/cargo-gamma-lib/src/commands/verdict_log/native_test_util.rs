// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub const TESTING_PROGRESS_LOG: &str = super::TESTING_PROGRESS_LOG;
pub const TEST_SELECTION_LOG: &str = super::TEST_SELECTION_LOG;

#[derive(Debug, Default)]
pub struct Journal(VerdictLog);

impl Journal {
    pub fn resume_with_progress(
        &mut self,
        scratch: &Utf8Path,
        limit: usize,
        inspect: impl FnOnce(&mut File),
        metadata: impl FnOnce(&File) -> std::io::Result<std::fs::Metadata>,
        progress: impl FnOnce(File, &Utf8Path) -> crate::Result<File>,
    ) -> crate::Result<()> {
        self.0.resume_with_progress(scratch, limit, inspect, metadata, progress)
    }

    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        matches!(self.0, VerdictLog::Disabled)
    }

    pub fn resume_progress_file(file: File, path: &Utf8Path) -> crate::Result<File> {
        VerdictLog::resume_progress_file(file, path)
    }

    pub fn resume_selection_file(
        file: File,
        path: &Utf8Path,
        limit: usize,
        inspect: impl FnOnce(&mut File),
        metadata: impl FnOnce(&File) -> std::io::Result<std::fs::Metadata>,
    ) -> crate::Result<(File, usize, bool)> {
        VerdictLog::resume_selection_file(file, path, limit, inspect, metadata)
    }
}
