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
        self.resume_with_inspector(scratch, MAX_SELECTION_LOG_BYTES, |_| {})
    }

    // Observes or mutates the selection journal after repair, before its retained-tail read and
    // Writing-state installation; native opening and recovery remain part of resume.
    fn resume_with_inspector(&mut self, scratch: &Utf8Path, limit: usize, inspect: impl FnOnce(&mut File)) -> crate::Result<()> {
        self.resume_with_metadata(scratch, limit, inspect, File::metadata)
    }

    // Supplies metadata for the already-open selection journal before repair scans its tail,
    // retaining native file opening, repair, post-repair inspection, and state installation.
    fn resume_with_metadata(
        &mut self,
        scratch: &Utf8Path,
        limit: usize,
        inspect: impl FnOnce(&mut File),
        metadata: impl FnOnce(&File) -> std::io::Result<std::fs::Metadata>,
    ) -> crate::Result<()> {
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
        let length = Self::repair_log_tail_with_metadata(&mut selection_file, metadata).map_err(|cause| {
            error!("could not recover test-selection log `{selection_path}`; check that it is writable").caused_by(cause)
        })?;
        inspect(&mut selection_file);
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
            selection_truncated: tail == SELECTION_TRUNCATION_MARKER || selection_bytes >= limit,
            replay_prepared: false,
        };
        Ok(())
    }

    fn repair_log_tail(file: &mut File) -> std::io::Result<u64> {
        Self::repair_log_tail_with_metadata(file, File::metadata)
    }

    // Obtains the open journal's length through the provider before ordinary tail scanning and repair.
    fn repair_log_tail_with_metadata(
        file: &mut File,
        metadata: impl FnOnce(&File) -> std::io::Result<std::fs::Metadata>,
    ) -> std::io::Result<u64> {
        let length = metadata(file)?.len();
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
            } => Self::write_selection(
                selection_file,
                attempt,
                selection_buffer,
                selection_bytes,
                selection_truncated,
                limit,
            )
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

    fn write_selection(
        writer: &mut impl std::io::Write,
        attempt: &SelectionAttempt,
        buffer: &mut Vec<u8>,
        bytes: &mut usize,
        truncated: &mut bool,
        limit: usize,
    ) -> std::io::Result<()> {
        if *truncated {
            return Ok(());
        }
        buffer.clear();
        serde_json::to_writer(&mut *buffer, attempt).map_err(std::io::Error::other)?;
        buffer.push(b'\n');
        if bytes.saturating_add(buffer.len()).saturating_add(SELECTION_TRUNCATION_MARKER.len()) > limit {
            writer.write_all(SELECTION_TRUNCATION_MARKER)?;
            writer.flush()?;
            *bytes = bytes.saturating_add(SELECTION_TRUNCATION_MARKER.len());
            *truncated = true;
        } else {
            writer.write_all(buffer)?;
            writer.flush()?;
            *bytes = bytes.saturating_add(buffer.len());
        }
        Ok(())
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

    use super::{MAX_SELECTION_LOG_BYTES, SELECTION_TRUNCATION_MARKER, TEST_SELECTION_LOG, TESTING_PROGRESS_LOG, VerdictLog};
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
    #[cfg(all(feature = "system-tests", not(miri)))]
    fn survivor_resumed_selection_stops_at_the_exact_cap_without_requiring_a_truncation_marker() {
        let directory = crate::testing::workdir("selection-resume-cap-");
        let scratch = Utf8Path::from_path(directory.path()).expect("fixture root is UTF-8");
        let retained = b"{\"retained\":true}\n";
        for cap in [retained.len() - 1, retained.len(), retained.len() + 1] {
            fs::write(scratch.join(TEST_SELECTION_LOG), retained).expect("a complete retained journal line can be written");
            let mut log = VerdictLog::default();
            log.resume_with_inspector(scratch, cap, |_| {})
                .expect("the actual native journal can be resumed");
            let VerdictLog::Writing {
                selection_bytes,
                selection_truncated,
                ..
            } = &log
            else {
                panic!("successful resume installs the opened native files");
            };
            assert_eq!(*selection_bytes, retained.len());
            assert_eq!(*selection_truncated, retained.len() >= cap);
            log.record_selection(&attempt());
            log.finish().expect("the journal suffix can be flushed");
            let actual = fs::read(scratch.join(TEST_SELECTION_LOG)).expect("the resulting journal remains readable");
            if cap <= retained.len() {
                assert_eq!(actual, retained, "a cap without the marker still forbids additional appends");
            } else {
                let mut record = serde_json::to_vec(&attempt()).expect("the selection attempt serializes");
                record.push(b'\n');
                assert_eq!(actual, [retained.as_slice(), &record].concat());
            }
        }
    }

    #[test]
    #[cfg(all(feature = "system-tests", not(miri)))]
    fn survivor_resumed_selection_returns_the_opened_file_metadata_provider_failure_before_tail_inspection() {
        let directory = crate::testing::workdir("selection-open-metadata-fault-");
        let scratch = Utf8Path::from_path(directory.path()).expect("fixture root is UTF-8");
        let path = scratch.join(TEST_SELECTION_LOG);
        let retained = b"{\"retained\":true}\n";
        fs::write(&path, retained).expect("the actual selected journal can be created");
        let called = std::cell::Cell::new(false);
        let mut log = VerdictLog::default();
        let error = log
            .resume_with_metadata(
                scratch,
                MAX_SELECTION_LOG_BYTES,
                |_| panic!("a metadata failure must return before the retained-tail inspector"),
                |opened| {
                    called.set(true);
                    assert_eq!(
                        opened
                            .metadata()
                            .expect("the provider receives the already-opened actual journal")
                            .len(),
                        retained.len() as u64
                    );
                    fs::metadata(scratch.join("missing-metadata-provider-result"))
                },
            )
            .expect_err("the native metadata-provider error must propagate rather than panic");
        assert!(called.get());
        assert!(error.to_string().contains("could not recover test-selection log"), "{error}");
        assert!(error.to_string().contains(path.as_str()), "{error}");
        assert!(matches!(log, VerdictLog::Disabled));
        assert_eq!(
            fs::read(path).expect("the unopened append phase did not alter retained bytes"),
            retained
        );
    }

    #[test]
    #[cfg(all(feature = "system-tests", not(miri)))]
    fn survivor_resumed_selection_returns_a_native_tail_read_race_without_installing_partial_state() {
        let directory = crate::testing::workdir("selection-tail-race-");
        let scratch = Utf8Path::from_path(directory.path()).expect("fixture root is UTF-8");
        fs::write(scratch.join(TEST_SELECTION_LOG), "{\"retained\":true}\n").expect("the original journal has a complete line");
        let mut log = VerdictLog::default();
        let failure = log
            .resume_with_inspector(scratch, MAX_SELECTION_LOG_BYTES, |file| {
                file.set_len(0)
                    .expect("a competing native truncation can remove the inspected tail");
            })
            .expect_err("a native UnexpectedEof after metadata inspection must be returned, not unwrapped");
        assert!(failure.to_string().contains("could not inspect test-selection log"), "{failure}");
        assert!(matches!(log, VerdictLog::Disabled));
    }

    #[test]
    #[cfg(all(feature = "system-tests", not(miri)))]
    fn survivor_record_selection_forwards_the_exact_production_retention_budget_at_both_adjacent_boundaries() {
        let directory = crate::testing::workdir("selection-production-budget-");
        let scratch = Utf8Path::from_path(directory.path()).expect("fixture root is UTF-8");
        let mut record = serde_json::to_vec(&attempt()).expect("the selection attempt serializes");
        record.push(b'\n');
        for extra in [0, 1] {
            let mut log = VerdictLog::default();
            log.start(scratch).expect("the bounded test journal can be created");
            let prefix_bytes = MAX_SELECTION_LOG_BYTES - record.len() - SELECTION_TRUNCATION_MARKER.len() + extra;
            let VerdictLog::Writing { selection_bytes, .. } = &mut log else {
                panic!("start installs native writable files");
            };
            *selection_bytes = prefix_bytes;
            log.record_selection(&attempt());
            let VerdictLog::Writing {
                selection_bytes,
                selection_truncated,
                ..
            } = &log
            else {
                panic!("the writable journal does not fail");
            };
            let suffix = if extra == 0 {
                record.as_slice()
            } else {
                SELECTION_TRUNCATION_MARKER
            };
            assert_eq!(*selection_truncated, extra == 1);
            assert_eq!(*selection_bytes, prefix_bytes + suffix.len());
            log.finish().expect("the bounded appended suffix is flushed");
            assert_eq!(
                fs::read(scratch.join(TEST_SELECTION_LOG)).expect("the actual suffix is readable"),
                suffix
            );
        }
    }

    #[test]
    #[cfg(all(feature = "system-tests", not(miri)))]
    fn survivor_verdict_write_failure_is_retained_for_finish_instead_of_reverting_to_success() {
        let directory = crate::testing::workdir("verdict-native-write-error-");
        let scratch = Utf8Path::from_path(directory.path()).expect("fixture root is UTF-8");
        let mut log = VerdictLog::default();
        log.start(scratch).expect("the actual journal can be created");
        let path = scratch.join(TESTING_PROGRESS_LOG);
        let VerdictLog::Writing { file, .. } = &mut log else {
            panic!("start installs the native writable file");
        };
        *file = File::open(&path).expect("the same native file can be opened read-only");
        log.record(&crate::fixtures::mutant());
        assert!(matches!(log, VerdictLog::Failed { .. }));
        log.record(&crate::fixtures::mutant());
        let failure = log
            .finish()
            .expect_err("the original native write failure must survive subsequent verdicts");
        assert!(failure.to_string().contains("could not write the campaign log"), "{failure}");
        assert!(failure.to_string().contains(path.as_str()), "{failure}");
        assert!(matches!(log, VerdictLog::Disabled));
        assert_eq!(fs::read(path).expect("the read-only fixture remains readable"), b"");
    }

    #[test]
    #[cfg(all(feature = "system-tests", not(miri)))]
    fn survivor_journal_creation_and_reopening_errors_are_returned_without_replacing_state() {
        for selection in [false, true] {
            let directory = crate::testing::workdir("journal-error-survivors-");
            let scratch = Utf8Path::from_path(directory.path()).unwrap();
            let blocked = scratch.join(if selection { TEST_SELECTION_LOG } else { TESTING_PROGRESS_LOG });
            fs::create_dir(&blocked).unwrap();
            let mut log = VerdictLog::default();
            assert!(log.start(scratch).is_err());
            assert!(matches!(log, VerdictLog::Disabled));
            assert!(log.resume(scratch).is_err());
            assert!(matches!(log, VerdictLog::Disabled));
        }
    }

    #[test]
    fn survivor_selection_retention_updates_byte_accounting_once_and_keeps_the_marker_reserved() {
        let mut record = serde_json::to_vec(&attempt()).unwrap();
        record.push(b'\n');
        for limit in [
            record.len() + SELECTION_TRUNCATION_MARKER.len() - 1,
            record.len() + SELECTION_TRUNCATION_MARKER.len(),
        ] {
            let mut output = Vec::new();
            let mut buffer = Vec::new();
            let mut bytes = 0;
            let mut truncated = false;
            VerdictLog::write_selection(&mut output, &attempt(), &mut buffer, &mut bytes, &mut truncated, limit).unwrap();
            let expected: &[u8] = if limit == record.len() + SELECTION_TRUNCATION_MARKER.len() {
                &record
            } else {
                SELECTION_TRUNCATION_MARKER
            };
            assert_eq!(output, expected);
            assert_eq!(bytes, expected.len());
            assert_eq!(truncated, expected == SELECTION_TRUNCATION_MARKER);
            VerdictLog::write_selection(&mut output, &attempt(), &mut buffer, &mut bytes, &mut truncated, limit).unwrap();
            let once_truncated = output.clone();
            VerdictLog::write_selection(&mut output, &attempt(), &mut buffer, &mut bytes, &mut truncated, limit).unwrap();
            assert!(truncated);
            assert_eq!(bytes, output.len());
            assert_eq!(output, once_truncated, "a retained marker prevents any further writes");
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
