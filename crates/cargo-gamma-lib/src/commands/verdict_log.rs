// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs::File;
use std::io::Write as _;

use camino::{Utf8Path, Utf8PathBuf};

use super::console_events::mutant_detail;
use crate::error::error;
use crate::exec::SelectionAttempt;
use crate::model::Mutant;
use crate::report::Styler;

pub(super) const TESTING_PROGRESS_LOG: &str = "gamma-progress.log";
pub(super) const TEST_SELECTION_LOG: &str = "gamma-selection.jsonl";

#[derive(Debug, Default)]
pub(super) enum VerdictLog {
    #[default]
    Disabled,
    Writing {
        file: File,
        path: Utf8PathBuf,
        selection_file: File,
        selection_path: Utf8PathBuf,
    },
    Failed {
        path: Utf8PathBuf,
        cause: std::io::Error,
    },
}

impl VerdictLog {
    pub(super) fn start(&mut self, scratch: &Utf8Path) -> crate::Result<()> {
        let path = scratch.join(TESTING_PROGRESS_LOG);
        let file = File::create(path.as_std_path())
            .map_err(|cause| error!("could not create the testing progress log at `{path}`").caused_by(cause))?;
        let selection_path = scratch.join(TEST_SELECTION_LOG);
        let selection_file = File::create(selection_path.as_std_path())
            .map_err(|cause| error!("could not create the test-selection log at `{selection_path}`").caused_by(cause))?;

        *self = Self::Writing {
            file,
            path,
            selection_file,
            selection_path,
        };

        Ok(())
    }

    /// Writes and flushes one verdict, retaining the first failure for [`Self::finish`].
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub(super) fn record(&mut self, mutant: &Mutant) {
        let failed = match self {
            Self::Writing { file, .. } => {
                let label = Styler::new(false).outcome(mutant.outcome);
                let detail = mutant_detail(mutant);

                writeln!(file, "{label} {detail}").and_then(|()| file.flush()).err()
            }
            Self::Disabled | Self::Failed { .. } => None,
        };

        let Some(cause) = failed else { return };
        let path = match self {
            Self::Writing { path, .. } => path.clone(),
            Self::Disabled | Self::Failed { .. } => return,
        };

        *self = Self::Failed { path, cause };
    }

    /// Writes and flushes one structured selection attempt.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub(super) fn record_selection(&mut self, attempt: &SelectionAttempt) {
        let failed = match self {
            Self::Writing { selection_file, .. } => serde_json::to_writer(&mut *selection_file, attempt)
                .map_err(std::io::Error::other)
                .and_then(|()| writeln!(selection_file))
                .and_then(|()| selection_file.flush())
                .err(),
            Self::Disabled | Self::Failed { .. } => None,
        };

        let Some(cause) = failed else { return };
        let path = match self {
            Self::Writing { selection_path, .. } => selection_path.clone(),
            Self::Disabled | Self::Failed { .. } => return,
        };

        *self = Self::Failed { path, cause };
    }

    // #[gamma::skip(all, reason = "finish only flushes an optional best-effort diagnostic log; absent logs and already-flushed buffers have the same command result")]
    pub(super) fn finish(&mut self) -> crate::Result<()> {
        let previous = core::mem::take(self);

        match previous {
            Self::Failed { path, cause } => Err(error!("could not write the campaign log at `{path}`").caused_by(cause)),
            Self::Disabled | Self::Writing { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};

    use camino::Utf8Path;

    use super::{TEST_SELECTION_LOG, VerdictLog};
    use crate::exec::{SelectionAttempt, SelectionResult, SelectionTier};

    fn attempt() -> SelectionAttempt {
        SelectionAttempt {
            ordinal: 7,
            tier: SelectionTier::Item,
            package: "subject".to_owned(),
            target: "lib".to_owned(),
            test: Some("tests::caught".to_owned()),
            rank: 2,
            result: SelectionResult::Miss,
            all_tests: false,
            elapsed_ms: 13,
            fallback_ms: 89,
        }
    }

    #[test]
    fn selection_attempts_are_immediately_visible_as_json_lines() {
        let directory = tempfile::tempdir().expect("temporary directory creation should succeed");
        let scratch = Utf8Path::from_path(directory.path()).expect("temporary paths should be UTF-8");
        let mut log = VerdictLog::default();
        log.start(scratch).expect("campaign log creation should succeed");

        log.record_selection(&attempt());

        let text = fs::read_to_string(scratch.join(TEST_SELECTION_LOG)).expect("the flushed selection log should be readable immediately");
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        let record: serde_json::Value = serde_json::from_str(lines[0]).expect("selection record should be valid JSON");
        assert_eq!(record["ordinal"], 7);
        assert_eq!(record["tier"], "item");
        assert_eq!(record["rank"], 2);
        assert_eq!(record["result"], "miss");
        assert_eq!(record["allTests"], false);
        assert_eq!(record["elapsedMs"], 13);
        assert_eq!(record["fallbackMs"], 89);
    }

    #[test]
    fn selection_write_failures_are_reported_at_finish() {
        let directory = tempfile::tempdir().expect("temporary directory creation should succeed");
        let scratch = Utf8Path::from_path(directory.path()).expect("temporary paths should be UTF-8");
        let progress_path = scratch.join("progress.log");
        let selection_path = scratch.join(TEST_SELECTION_LOG);
        let file = File::create(progress_path.as_std_path()).expect("progress log creation should succeed");
        File::create(selection_path.as_std_path()).expect("selection log creation should succeed");
        let selection_file = File::open(selection_path.as_std_path()).expect("selection log should open read-only");
        let mut log = VerdictLog::Writing {
            file,
            path: progress_path,
            selection_file,
            selection_path: selection_path.clone(),
        };

        log.record_selection(&attempt());

        let error = log.finish().expect_err("a read-only selection log should fail when flushed");
        assert!(error.to_string().contains(selection_path.as_str()));
    }
}
