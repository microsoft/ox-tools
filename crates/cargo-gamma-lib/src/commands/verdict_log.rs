// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, Write as _};

use camino::{Utf8Path, Utf8PathBuf};

use super::console_events::mutant_detail;
use crate::error::error;
use crate::exec::SelectionAttempt;
use crate::model::Mutant;
use crate::report::Styler;

pub(super) const TESTING_PROGRESS_LOG: &str = "gamma-progress.log";
pub(super) const TEST_SELECTION_LOG: &str = "gamma-selection.jsonl";
const MAX_SELECTION_LOG_BYTES: usize = 64 * 1024 * 1024;
const SELECTION_TRUNCATION_MARKER: &[u8] = b"{\"truncated\":true,\"reason\":\"selection journal reached its 64 MiB retention limit\"}\n";

#[derive(Debug, Default)]
pub(super) enum VerdictLog {
    #[default]
    Disabled,
    Writing {
        file: File,
        path: Utf8PathBuf,
        selection_file: File,
        selection_path: Utf8PathBuf,
        selection_buffer: Vec<u8>,
        selection_bytes: usize,
        selection_truncated: bool,
        replay_prepared: bool,
    },
    Failed {
        path: Utf8PathBuf,
        cause: std::io::Error,
    },
}

impl VerdictLog {
    pub(super) fn resume(&mut self, scratch: &Utf8Path) -> crate::Result<()> {
        let path = scratch.join(TESTING_PROGRESS_LOG);
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|cause| error!("could not reopen testing progress log `{path}`").caused_by(cause))?;
        Self::repair_log_tail(&mut file)
            .map_err(|cause| error!("could not recover testing progress log `{path}`; check that it is writable").caused_by(cause))?;
        drop(file);
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|cause| error!("could not reopen testing progress log `{path}`").caused_by(cause))?;
        let selection_path = scratch.join(TEST_SELECTION_LOG);
        let mut selection_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&selection_path)
            .map_err(|cause| error!("could not reopen test-selection log `{selection_path}`").caused_by(cause))?;
        let length = Self::repair_log_tail(&mut selection_file).map_err(|cause| {
            error!("could not recover test-selection log `{selection_path}`; check that it is writable").caused_by(cause)
        })?;
        let selection_bytes = usize::try_from(length).unwrap_or(usize::MAX);
        let tail_length = length.min(SELECTION_TRUNCATION_MARKER.len() as u64);
        let mut tail = vec![0; selection_bytes.min(SELECTION_TRUNCATION_MARKER.len())];
        selection_file
            .seek(std::io::SeekFrom::Start(length - tail_length))
            .and_then(|_| selection_file.read_exact(&mut tail))
            .map_err(|cause| error!("could not inspect test-selection log `{selection_path}`").caused_by(cause))?;
        // Windows append-only handles cannot truncate an interrupted tail.
        drop(selection_file);
        let selection_file = OpenOptions::new()
            .append(true)
            .open(&selection_path)
            .map_err(|cause| error!("could not reopen test-selection log `{selection_path}`").caused_by(cause))?;
        *self = Self::Writing {
            file,
            path,
            selection_file,
            selection_path,
            selection_buffer: Vec::new(),
            selection_bytes,
            selection_truncated: tail == SELECTION_TRUNCATION_MARKER || selection_bytes >= MAX_SELECTION_LOG_BYTES,
            replay_prepared: false,
        };
        Ok(())
    }

    fn repair_log_tail(file: &mut File) -> std::io::Result<u64> {
        let length = file.metadata()?.len();
        let mut end = length;
        let mut buffer = [0; 4096];
        while end > 0 {
            let start = end.saturating_sub(buffer.len() as u64);
            let count = usize::try_from(end - start).expect("the read length is bounded by the 4096-byte buffer");
            file.seek(std::io::SeekFrom::Start(start))?;
            file.read_exact(&mut buffer[..count])?;
            if let Some(newline) = buffer[..count].iter().rposition(|byte| *byte == b'\n') {
                end = start + newline as u64 + 1;
                break;
            }
            end = start;
        }
        if end != length {
            file.set_len(end)?;
            file.sync_all()?;
        }
        Ok(end)
    }

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
            selection_buffer: Vec::new(),
            selection_bytes: 0,
            selection_truncated: false,
            replay_prepared: true,
        };

        Ok(())
    }

    pub(super) fn record_prepared(&mut self, mutant: &Mutant) {
        if matches!(self, Self::Writing { replay_prepared: true, .. }) {
            self.record(mutant);
        }
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
        self.record_selection_with_limit(attempt, MAX_SELECTION_LOG_BYTES);
    }

    fn record_selection_with_limit(&mut self, attempt: &SelectionAttempt, limit: usize) {
        let failed = match self {
            Self::Writing {
                selection_file,
                selection_buffer,
                selection_bytes,
                selection_truncated,
                ..
            } => {
                if *selection_truncated {
                    None
                } else {
                    selection_buffer.clear();
                    serde_json::to_writer(&mut *selection_buffer, attempt)
                        .map_err(std::io::Error::other)
                        .and_then(|()| {
                            selection_buffer.push(b'\n');
                            if selection_bytes
                                .saturating_add(selection_buffer.len())
                                .saturating_add(SELECTION_TRUNCATION_MARKER.len())
                                > limit
                            {
                                selection_file.write_all(SELECTION_TRUNCATION_MARKER)?;
                                selection_file.flush()?;
                                *selection_bytes = selection_bytes.saturating_add(SELECTION_TRUNCATION_MARKER.len());
                                *selection_truncated = true;
                            } else {
                                selection_file.write_all(selection_buffer)?;
                                selection_file.flush()?;
                                *selection_bytes = selection_bytes.saturating_add(selection_buffer.len());
                            }
                            Ok(())
                        })
                        .err()
                }
            }
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
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::fs::{self, File};
    use std::io::Write as _;

    use camino::Utf8Path;

    use super::{SELECTION_TRUNCATION_MARKER, TEST_SELECTION_LOG, TESTING_PROGRESS_LOG, VerdictLog};
    use crate::exec::{SelectionAttempt, SelectionResult, SelectionTier};

    #[test]
    fn resumed_progress_discards_only_the_interrupted_verdict_tail() {
        for retained in [false, true] {
            for suffix in [&b"Killed partial"[..], &vec![b'x'; 8200]] {
                let directory = tempfile::tempdir().unwrap();
                let scratch = Utf8Path::from_path(directory.path()).unwrap();
                let mutant = crate::fixtures::mutant();
                let expected_line = format!(
                    "{} {}\n",
                    crate::report::Styler::new(false).outcome(mutant.outcome),
                    super::mutant_detail(&mutant)
                );
                let mut log = VerdictLog::default();
                log.start(scratch).unwrap();
                if retained {
                    log.record(&mutant);
                }
                let path = scratch.join(TESTING_PROGRESS_LOG);
                let prefix = fs::read(&path).unwrap();
                let VerdictLog::Writing { file, .. } = &mut log else {
                    panic!("starting the log must install a writable handle");
                };
                file.write_all(suffix).unwrap();
                file.flush().unwrap();
                log.finish().unwrap();

                log.resume(scratch).unwrap();
                log.record_prepared(&mutant);
                assert_eq!(fs::read(&path).unwrap(), prefix);
                log.record(&mutant);
                log.finish().unwrap();
                let complete = fs::read(&path).unwrap();
                assert_eq!(complete, [prefix.as_slice(), expected_line.as_bytes()].concat());
                if retained {
                    assert_eq!(complete, [prefix.as_slice(), prefix.as_slice()].concat());
                }
                log.resume(scratch).unwrap();
                log.finish().unwrap();
                assert_eq!(fs::read(&path).unwrap(), complete);
            }
        }
    }

    #[test]
    fn resumed_measurement_never_replays_existing_verdict_lines() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = Utf8Path::from_path(directory.path()).unwrap();
        let mut first = crate::fixtures::mutant();
        first.outcome = crate::model::Outcome::Killed;
        let mut second = first.clone();
        second.outcome = crate::model::Outcome::Survived;
        let mut log = VerdictLog::default();
        log.start(scratch).unwrap();
        log.record_prepared(&first);
        log.finish().unwrap();
        let prefix = fs::read(scratch.join(TESTING_PROGRESS_LOG)).unwrap();
        assert!(!prefix.is_empty());
        log.resume(scratch).unwrap();
        log.record_prepared(&first);
        assert_eq!(fs::read(scratch.join(TESTING_PROGRESS_LOG)).unwrap(), prefix);
        log.record(&second);
        log.finish().unwrap();
        let complete = fs::read(scratch.join(TESTING_PROGRESS_LOG)).unwrap();
        assert_eq!(complete.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).count(), 2);
        log.resume(scratch).unwrap();
        for mutant in [&first, &second] {
            log.record_prepared(mutant);
        }
        log.finish().unwrap();
        assert_eq!(fs::read(scratch.join(TESTING_PROGRESS_LOG)).unwrap(), complete);
    }

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
            selection_buffer: Vec::new(),
            selection_bytes: 0,
            selection_truncated: false,
            replay_prepared: true,
        };

        log.record_selection(&attempt());

        let error = log.finish().expect_err("a read-only selection log should fail when flushed");
        assert!(error.to_string().contains(selection_path.as_str()));
    }

    #[test]
    fn selection_retention_reserves_one_truncation_marker_at_the_limit() {
        let directory = tempfile::tempdir().expect("temporary directory creation should succeed");
        let scratch = Utf8Path::from_path(directory.path()).expect("temporary paths should be UTF-8");
        let mut log = VerdictLog::default();
        log.start(scratch).expect("campaign log creation should succeed");

        let mut record = serde_json::to_vec(&attempt()).expect("selection attempt should serialize");
        record.push(b'\n');
        let limit = record.len() + SELECTION_TRUNCATION_MARKER.len();

        log.record_selection_with_limit(&attempt(), limit);
        log.record_selection_with_limit(&attempt(), limit);
        log.record_selection_with_limit(&attempt(), limit);

        let bytes = fs::read(scratch.join(TEST_SELECTION_LOG)).expect("selection log should be readable");
        assert_eq!(bytes, [record.as_slice(), SELECTION_TRUNCATION_MARKER].concat());
    }

    #[test]
    fn resume_appends_both_logs_and_keeps_an_existing_selection_limit() {
        let directory = tempfile::tempdir().unwrap();
        let scratch = Utf8Path::from_path(directory.path()).unwrap();
        let mut log = VerdictLog::default();
        log.start(scratch).unwrap();
        log.record(&crate::fixtures::mutant());
        log.record_selection(&attempt());
        log.finish().unwrap();
        let progress_path = scratch.join(super::TESTING_PROGRESS_LOG);
        let selection_path = scratch.join(TEST_SELECTION_LOG);
        let progress = fs::read(&progress_path).unwrap();
        let selection = fs::read(&selection_path).unwrap();
        log.resume(scratch).unwrap();
        log.record(&crate::fixtures::mutant());
        log.record_selection(&attempt());
        log.finish().unwrap();
        assert_eq!(
            fs::read(&progress_path).unwrap(),
            [progress.as_slice(), progress.as_slice()].concat()
        );
        assert_eq!(
            fs::read(&selection_path).unwrap(),
            [selection.as_slice(), selection.as_slice()].concat()
        );
        fs::write(&selection_path, [selection.as_slice(), SELECTION_TRUNCATION_MARKER].concat()).unwrap();
        let truncated = fs::read(&selection_path).unwrap();
        log.resume(scratch).unwrap();
        log.record_selection(&attempt());
        log.finish().unwrap();
        assert_eq!(fs::read(selection_path).unwrap(), truncated);
    }

    #[test]
    fn resume_creates_missing_auxiliary_logs_without_truncating_the_other() {
        for missing in [super::TESTING_PROGRESS_LOG, TEST_SELECTION_LOG] {
            let directory = tempfile::tempdir().unwrap();
            let scratch = Utf8Path::from_path(directory.path()).unwrap();
            let mut log = VerdictLog::default();
            log.start(scratch).unwrap();
            log.record(&crate::fixtures::mutant());
            log.record_selection(&attempt());
            log.finish().unwrap();
            let other = if missing == TEST_SELECTION_LOG {
                super::TESTING_PROGRESS_LOG
            } else {
                TEST_SELECTION_LOG
            };
            let retained = fs::read(scratch.join(other)).unwrap();
            fs::remove_file(scratch.join(missing)).unwrap();
            log.resume(scratch).unwrap();
            log.finish().unwrap();
            assert!(scratch.join(missing).is_file());
            assert_eq!(fs::read(scratch.join(other)).unwrap(), retained);
        }
    }

    #[test]
    fn resume_repairs_torn_selection_lines_and_retention_markers() {
        let mut record = serde_json::to_vec(&attempt()).unwrap();
        record.push(b'\n');
        for suffix in [&b"{\"ordinal\":"[..], &vec![b'x'; 8200], &SELECTION_TRUNCATION_MARKER[..20]] {
            let directory = tempfile::tempdir().unwrap();
            let scratch = Utf8Path::from_path(directory.path()).unwrap();
            let path = scratch.join(TEST_SELECTION_LOG);
            fs::write(&path, [record.as_slice(), suffix].concat()).unwrap();
            let mut log = VerdictLog::default();
            log.resume(scratch).unwrap();
            log.record_selection(&attempt());
            log.finish().unwrap();
            let bytes = fs::read(&path).unwrap();
            assert_eq!(bytes, [record.as_slice(), record.as_slice()].concat());
            for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
                serde_json::from_slice::<serde_json::Value>(line).unwrap();
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let scratch = Utf8Path::from_path(directory.path()).unwrap();
        fs::write(scratch.join(TEST_SELECTION_LOG), b"{\"partial\":").unwrap();
        let mut log = VerdictLog::default();
        log.resume(scratch).unwrap();
        log.record_selection(&attempt());
        log.finish().unwrap();
        assert_eq!(fs::read(scratch.join(TEST_SELECTION_LOG)).unwrap(), record);
    }
}
